//! Opening connections: options, TLS, engine detection and session setup.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mysql_async::prelude::Queryable;
use mysql_async::{
    ClientIdentity, Conn, DriverError as MyDriverError, Error as MyError, Opts, OptsBuilder,
    SslOpts,
};
use tokio::sync::OnceCell;
use tokio::time::{Instant, timeout_at};
use varve_core::Engine;
use varve_driver::{ConnectParams, DriverError, ErrorKind, ServerInfo, TlsMode, parse_version};

use crate::error::{connect_error, session_error};
use crate::guard;
use crate::tracking;

/// Host name that `VerifyCa` hands to the TLS layer instead of the real one.
///
/// This workaround depends on the exact error text of `mysql_async` 0.37.1
/// and rustls 0.23, which is why `Cargo.toml` pins `mysql_async`.
/// `mysql_async` implements "check the chain, not the host name" by
/// skipping a verification error whose text contains `NotValidForName`.
/// rustls 0.23 reports that error as "certificate not valid for name
/// \"<expected name>\"", which no longer contains the marker, so the skip
/// never triggers for real host names. The certificate chain is checked
/// before the name, so with this name a certificate that chains to the
/// trusted CA fails only on the name check, the message quotes this name,
/// and the skip applies. Every other failure (unknown issuer, expiry, bad
/// signature) is still fatal, and if the message format ever changes the
/// connection fails instead of silently skipping checks. The cost is that
/// this name is sent as SNI: plain MySQL and MariaDB servers ignore it, but
/// proxies and gateways that route on SNI cannot route `VerifyCa`
/// connections.
const VERIFY_CA_SERVER_NAME: &str = "NotValidForName.verify-ca.invalid";

/// The statement that makes `TIMESTAMP` values UTC. It runs last, after
/// `init_sql`, so the driver knows the session time zone when the session
/// opens.
pub(crate) const UTC_TIME_ZONE: &str = "SET time_zone = '+00:00'";

/// The client character set of every session.
pub(crate) const UTF8MB4: &str = "SET NAMES utf8mb4";

/// The statement that makes a session read-only (also used to restore it).
pub(crate) const READ_ONLY: &str = "SET SESSION TRANSACTION READ ONLY";

/// `ER_UNKNOWN_SYSTEM_VARIABLE`.
const ER_UNKNOWN_SYSTEM_VARIABLE: u16 = 1193;

assert_not_debug!(Opened);

/// A freshly opened and prepared connection. Never `Debug`: it holds
/// `Opts` (see [`client_identity`]).
pub(crate) struct Opened {
    /// The connection.
    pub conn: Conn,
    /// Options that reach the same server again (used by cancel).
    pub opts: Opts,
    /// Detected server identity.
    pub info: ServerInfo,
    /// The server's id for the connection, from `CONNECTION_ID()`.
    pub connection_id: u64,
    /// For read-only sessions, the variable that holds the access mode:
    /// `transaction_read_only`, or `tx_read_only` on servers that only have
    /// that one (MariaDB before 11.1).
    pub read_only_variable: Option<&'static str>,
    /// The statements that set up session state tracking, when the server
    /// tracks (checked while connecting); run again to restore it.
    pub tracking: Option<Vec<String>>,
    /// The variables the server accepted for tracking (it may drop some,
    /// such as `collation_connection` on MariaDB 12).
    pub tracked_names: Vec<String>,
    /// The client character set after the setup (utf8mb4).
    pub charset: tracking::ClientCharset,
    /// The session's `sql_mode` after the setup.
    pub sql_mode: String,
    /// True when the connection runs over TLS.
    pub tls: bool,
}

/// What the session setup learned about the server.
struct Prepared {
    info: ServerInfo,
    connection_id: u64,
    setup: SessionSetup,
}

/// What [`prepare`] set up.
struct SessionSetup {
    read_only_variable: Option<&'static str>,
    tracking: Option<Vec<String>>,
    tracked_names: Vec<String>,
    charset: tracking::ClientCharset,
    sql_mode: String,
}

/// Connects, detects the engine and runs the session setup.
pub(crate) async fn open(params: &ConnectParams) -> Result<Opened, DriverError> {
    let endpoint = format!("{}:{}", params.host, params.port);
    let deadline = Instant::now() + params.connect_timeout;
    let base = base_opts(params);
    let ssl = ssl_opts(params).await?;
    let tls_requested = ssl.is_some();

    let (mut conn, opts, tls) = match login(base.clone().ssl_opts(ssl), deadline, &endpoint).await {
        Ok((conn, opts)) => (conn, opts, tls_requested),
        Err(LoginError::Driver(MyError::Driver(MyDriverError::NoClientSslFlagFromServer)))
            if params.tls.mode == TlsMode::Prefer =>
        {
            tracing::debug!(%endpoint, "server offers no TLS, continuing without it");
            let (conn, opts) = login(base.ssl_opts(None), deadline, &endpoint)
                .await
                .map_err(|error| error.into_driver_error(&endpoint))?;
            (conn, opts, false)
        }
        Err(error) => return Err(error.into_driver_error(&endpoint)),
    };

    // The connect timeout covers the whole setup, including `init_sql`.
    let prepared = match timeout_at(deadline, setup(&mut conn, params)).await {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => {
            disconnect_quietly(conn).await;
            return Err(error);
        }
        Err(_) => {
            // A statement may still be running: close without waiting for it.
            disconnect_quietly(conn).await;
            return Err(DriverError::new(
                ErrorKind::Timeout,
                format!("connecting to {endpoint} timed out during the session setup"),
            ));
        }
    };
    let info = prepared.info;
    if info.engine != params.engine {
        tracing::info!(
            profile = ?params.engine,
            detected = ?info.engine,
            "server engine differs from the connection profile"
        );
    }

    Ok(Opened {
        conn,
        opts,
        info,
        connection_id: prepared.connection_id,
        read_only_variable: prepared.setup.read_only_variable,
        tracking: prepared.setup.tracking,
        tracked_names: prepared.setup.tracked_names,
        charset: prepared.setup.charset,
        sql_mode: prepared.setup.sql_mode,
        tls,
    })
}

/// Detects the server, reads the connection id and prepares the session.
async fn setup(conn: &mut Conn, params: &ConnectParams) -> Result<Prepared, DriverError> {
    let (info, connection_id) = detect(conn).await?;
    let setup = prepare(conn, params, &info).await?;
    Ok(Prepared {
        info,
        connection_id,
        setup,
    })
}

/// Opens a plain connection with options from [`open`], for `KILL QUERY`.
pub(crate) async fn reconnect(opts: Opts, connect_timeout: Duration) -> Result<Conn, DriverError> {
    let endpoint = format!("{}:{}", opts.ip_or_hostname(), opts.tcp_port());
    login(opts, Instant::now() + connect_timeout, &endpoint)
        .await
        .map(|(conn, _)| conn)
        .map_err(|error| error.into_driver_error(&endpoint))
}

/// How long a disconnect may take before the socket is simply closed.
const DISCONNECT_LIMIT: Duration = Duration::from_millis(250);

/// Disconnects, ignoring errors (used on paths that already failed or are
/// done).
///
/// `mysql_async` first reads whatever result is still pending, which for a
/// statement that still runs (an `init_sql` that hit the connect timeout)
/// would wait for it to finish. After [`DISCONNECT_LIMIT`] the connection
/// is dropped instead: it is already marked as disconnected by then, so the
/// drop just closes the socket.
pub(crate) async fn disconnect_quietly(conn: Conn) {
    match tokio::time::timeout(DISCONNECT_LIMIT, conn.disconnect()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!(%error, "disconnect failed"),
        Err(_) => tracing::debug!("disconnect took too long, closed the socket"),
    }
}

enum LoginError {
    Driver(MyError),
    Timeout,
}

impl LoginError {
    fn into_driver_error(self, endpoint: &str) -> DriverError {
        match self {
            LoginError::Driver(error) => connect_error(&error, endpoint),
            LoginError::Timeout => timeout_error(endpoint),
        }
    }
}

fn timeout_error(endpoint: &str) -> DriverError {
    DriverError::new(
        ErrorKind::Timeout,
        format!("connecting to {endpoint} timed out"),
    )
}

async fn login(
    opts: impl Into<Opts>,
    deadline: Instant,
    endpoint: &str,
) -> Result<(Conn, Opts), LoginError> {
    let opts: Opts = opts.into();
    tracing::debug!(%endpoint, tls = opts.ssl_opts().is_some(), "connecting");
    match timeout_at(deadline, Conn::new(opts.clone())).await {
        Ok(Ok(conn)) => Ok((conn, opts)),
        Ok(Err(error)) => Err(LoginError::Driver(error)),
        Err(_) => Err(LoginError::Timeout),
    }
}

/// Options shared by every connection of a session.
fn base_opts(params: &ConnectParams) -> OptsBuilder {
    let database = params
        .database
        .as_deref()
        .filter(|database| !database.is_empty())
        .map(str::to_owned);
    OptsBuilder::default()
        .ip_or_hostname(params.host.clone())
        .tcp_port(params.port)
        .user(params.username.clone())
        // The only place the password leaves its `Secret`.
        .pass(
            params
                .password
                .as_ref()
                .map(|secret| secret.expose().to_owned()),
        )
        .db_name(database)
        // Never switch to the Unix socket behind the user's back: it would
        // bypass TLS and the endpoint the user chose.
        .prefer_socket(false)
        .tcp_nodelay(true)
        .connect_attribute("program_name", params.application_name.clone())
}

/// TLS options for the requested mode. Multi statements are always enabled
/// by `mysql_async`, so they need no option here.
async fn ssl_opts(params: &ConnectParams) -> Result<Option<SslOpts>, DriverError> {
    let tls = &params.tls;
    if tls.mode == TlsMode::Disable {
        return Ok(None);
    }
    ensure_crypto_provider();

    let mut ssl = SslOpts::default();
    if let Some(identity) = client_identity(tls, params.connect_timeout).await? {
        ssl = ssl.with_client_identity(Some(identity));
    }

    match tls.mode {
        TlsMode::Disable => return Ok(None),
        TlsMode::Prefer | TlsMode::Require => {
            ssl = ssl
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true);
        }
        TlsMode::VerifyCa | TlsMode::VerifyFull => {
            ssl = with_roots(ssl, tls.ca_path.as_deref()).await?;
            if tls.mode == TlsMode::VerifyCa {
                ssl = ssl
                    .with_danger_skip_domain_validation(true)
                    .with_danger_tls_hostname_override(Some(VERIFY_CA_SERVER_NAME));
            } else if let Some(name) = tls.server_name.as_ref().filter(|name| !name.is_empty()) {
                ssl = ssl.with_danger_tls_hostname_override(Some(name.clone()));
            }
        }
    }
    Ok(Some(ssl))
}

/// Uses the rustls `ring` provider unless the process already chose one.
fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Fails only when another thread installed a provider first, which
        // is just as good.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

fn readable_file(path: &str, what: &str) -> Result<PathBuf, DriverError> {
    let path = Path::new(path);
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(path.to_path_buf()),
        Ok(_) => Err(DriverError::new(
            ErrorKind::Tls,
            format!("the {what} {} is not a file", path.display()),
        )),
        Err(error) => Err(DriverError::new(
            ErrorKind::Tls,
            format!("cannot read the {what} {}: {error}", path.display()),
        )),
    }
}

/// The client certificate and key for `mysql_async`. The key is read and,
/// when encrypted, decrypted here (see `varve_driver::tls_key`) and handed
/// over as unencrypted PEM in memory: `mysql_async` cannot decrypt keys.
///
/// Zeroization limit: Varve's own PEM text is wiped on drop, but the copy
/// handed over is owned by `mysql_async::Opts` (cloned for cancel and
/// reconnects, kept for the session's lifetime) and is **not wiped**:
/// `mysql_async` 0.37 neither zeroizes it nor accepts a rustls config.
/// `Opts` prints it in `Debug`, so the types holding `Opts` are guarded by
/// `assert_not_debug!`.
async fn client_identity(
    tls: &varve_driver::TlsParams,
    timeout: Duration,
) -> Result<Option<ClientIdentity>, DriverError> {
    let Some(identity) = varve_driver::tls_key::load_client_identity(tls, timeout).await? else {
        return Ok(None);
    };
    let cert = readable_file(&identity.cert_path, "client certificate")?;
    let key = identity.key.to_pem().as_bytes().to_vec();
    Ok(Some(ClientIdentity::new(cert.into(), key.into())))
}

/// Trust anchors: the CA file when given, otherwise the OS trust store.
async fn with_roots(ssl: SslOpts, ca_path: Option<&str>) -> Result<SslOpts, DriverError> {
    if let Some(ca_path) = ca_path.filter(|path| !path.is_empty()) {
        let ca = readable_file(ca_path, "CA file")?;
        return Ok(ssl
            .with_root_certs(vec![ca.into()])
            .with_disable_built_in_roots(true));
    }
    let native = native_roots().await;
    if native.is_empty() {
        // Keep the Mozilla roots built into `mysql_async`.
        Ok(ssl)
    } else {
        Ok(ssl
            .with_root_certs(native.iter().map(|der| der.clone().into()).collect())
            .with_disable_built_in_roots(true))
    }
}

/// The OS trust store, loaded once per process (DER encoded).
async fn native_roots() -> Arc<Vec<Vec<u8>>> {
    static ROOTS: OnceCell<Arc<Vec<Vec<u8>>>> = OnceCell::const_new();
    ROOTS
        .get_or_init(|| async {
            let loaded = tokio::task::spawn_blocking(|| {
                let result = rustls_native_certs::load_native_certs();
                for error in &result.errors {
                    tracing::warn!(%error, "could not load part of the OS trust store");
                }
                result
                    .certs
                    .into_iter()
                    .map(|cert| cert.as_ref().to_vec())
                    .collect::<Vec<_>>()
            })
            .await;
            Arc::new(loaded.unwrap_or_else(|error| {
                tracing::warn!(%error, "loading the OS trust store failed");
                Vec::new()
            }))
        })
        .await
        .clone()
}

/// Tells MySQL and MariaDB apart from `VERSION()` and `@@version_comment`.
pub(crate) fn detect_engine(version: &str, comment: &str) -> Engine {
    let mariadb = |text: &str| text.to_ascii_lowercase().contains("mariadb");
    if mariadb(version) || mariadb(comment) {
        Engine::Mariadb
    } else {
        Engine::Mysql
    }
}

/// Reads the server identity and the connection id.
///
/// The id comes from `CONNECTION_ID()`, not from the handshake: the
/// handshake id is only 32 bits wide and proxies may rewrite it, and
/// `KILL QUERY` must name the id the server itself uses.
async fn detect(conn: &mut Conn) -> Result<(ServerInfo, u64), DriverError> {
    let row: Option<(Option<String>, Option<String>, Option<u64>)> = conn
        .query_first("SELECT VERSION(), @@version_comment, CONNECTION_ID()")
        .await
        .map_err(|error| session_error(&error))?;
    let (version, comment, connection_id) = row.unwrap_or_default();
    let connection_id = connection_id.ok_or_else(|| {
        DriverError::new(ErrorKind::Protocol, "the server reported no connection id")
    })?;
    let version = version.unwrap_or_default();
    let comment = comment.unwrap_or_default();
    let engine = detect_engine(&version, &comment);
    let (major, minor, patch) = parse_version(&version).unwrap_or_else(|| {
        let (major, minor, patch) = conn.server_version();
        (u32::from(major), u32::from(minor), Some(u32::from(patch)))
    });
    Ok((
        ServerInfo {
            engine,
            version,
            major,
            minor,
            patch,
        },
        connection_id,
    ))
}

/// Session setup statements that run before `init_sql`, in order.
///
/// Read-only mode comes first so `init_sql` cannot write. The profile's
/// statement timeout also comes first; an `init_sql` that sets its own
/// timeout overrides it. The time zone ([`UTC_TIME_ZONE`]) runs after
/// `init_sql`.
pub(crate) fn setup_statements(params: &ConnectParams, engine: Engine) -> Vec<String> {
    let mut statements = Vec::new();
    if params.read_only {
        statements.push(READ_ONLY.to_owned());
    }
    if let Some(limit) = params.statement_timeout {
        statements.push(match engine {
            // MariaDB: seconds as a decimal, applies to every statement.
            Engine::Mariadb => format!(
                "SET SESSION max_statement_time = {}.{:03}",
                limit.as_secs(),
                limit.subsec_millis()
            ),
            // MySQL: milliseconds, applies to read-only SELECT statements only.
            _ => format!(
                "SET SESSION max_execution_time = {}",
                u32::try_from(limit.as_millis()).unwrap_or(u32::MAX)
            ),
        });
    }
    statements
}

/// Runs the session setup and `init_sql`.
async fn prepare(
    conn: &mut Conn,
    params: &ConnectParams,
    info: &ServerInfo,
) -> Result<SessionSetup, DriverError> {
    let engine = info.engine;
    let init_sql = params
        .init_sql
        .as_deref()
        .filter(|sql| !sql.trim().is_empty());
    if let Some(init_sql) = init_sql {
        guard::check_charset(init_sql, "the init SQL")?;
        if params.read_only {
            guard::check(init_sql, "the init SQL")?;
        }
    }
    for statement in setup_statements(params, engine) {
        conn.query_drop(statement.as_str())
            .await
            .map_err(|error| setup_error(&error))?;
    }
    let read_only_variable = if params.read_only {
        Some(read_only_state(conn).await?.0)
    } else {
        None
    };
    let tracking = match tracking_statements(conn, read_only_variable).await {
        Ok(statements) => statements,
        Err(error) if params.read_only => return Err(error),
        Err(error) => {
            tracing::debug!(%error, "session state tracking is not available");
            None
        }
    };
    if let Some(init_sql) = init_sql {
        // `query_drop` reads every result of a multi-statement script.
        conn.query_drop(init_sql).await.map_err(|error| {
            let mut error = session_error(&error);
            error.message = format!("init SQL failed: {}", error.message);
            error
        })?;
    }
    conn.query_drop(UTC_TIME_ZONE)
        .await
        .map_err(|error| setup_error(&error))?;
    // Tracking works when the server reported the time zone just set (it
    // reports a SET even when the value stays the same).
    let tracking =
        tracking.filter(|_| tracking::observe(conn.session_state_changes()).zone_reported);
    // Last: utf8mb4, whatever the server's init_connect or the init SQL
    // left. The session never runs in another character set.
    conn.query_drop(UTF8MB4)
        .await
        .map_err(|error| setup_error(&error))?;
    let charset = client_charset(conn).await?;
    if !charset.is_utf8mb4() {
        return Err(DriverError::new(
            ErrorKind::Unsupported,
            format!(
                "the server did not switch the session to utf8mb4 (client {}, connection {}, \
                 results {}, collation {}), which Varve needs",
                charset.client, charset.connection, charset.results, charset.collation
            ),
        ));
    }
    let tracked_names = if tracking.is_some() {
        tracked_list(conn).await?
    } else {
        Vec::new()
    };
    let sql_mode: Option<Option<String>> = conn
        .query_first("SELECT @@session.sql_mode")
        .await
        .map_err(|error| setup_error(&error))?;
    let sql_mode = sql_mode.flatten().unwrap_or_default();
    let Some(variable) = read_only_variable else {
        return Ok(SessionSetup {
            read_only_variable: None,
            tracking,
            tracked_names,
            charset,
            sql_mode,
        });
    };
    if tracking.is_none() {
        return Err(DriverError::new(
            ErrorKind::Unsupported,
            "the server does not report session state changes (session tracking), which a \
             read-only connection needs to notice a stored routine switching read-only mode \
             off; the read-only connection was not opened",
        ));
    }
    let (_, on) = read_only_state(conn).await?;
    if !on {
        // The guard should make this unreachable; the server has the last
        // word.
        return Err(DriverError::new(
            ErrorKind::ReadOnly,
            "the init SQL switched read-only mode off, so the read-only connection was not opened",
        ));
    }
    Ok(SessionSetup {
        read_only_variable: Some(variable),
        tracking,
        tracked_names,
        charset,
        sql_mode,
    })
}

/// The variables the server tracks for the session.
async fn tracked_list(conn: &mut Conn) -> Result<Vec<String>, DriverError> {
    let list: Option<Option<String>> = conn
        .query_first("SELECT @@session.session_track_system_variables")
        .await
        .map_err(|error| session_error(&error))?;
    Ok(list
        .flatten()
        .unwrap_or_default()
        .split(',')
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect())
}

/// Reads the client character set of the session.
pub(crate) async fn client_charset(
    conn: &mut Conn,
) -> Result<tracking::ClientCharset, DriverError> {
    type Row = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<Row> = conn
        .query_first(
            "SELECT @@session.character_set_client, @@session.character_set_connection, \
             @@session.character_set_results, @@session.collation_connection",
        )
        .await
        .map_err(|error| session_error(&error))?;
    let (client, connection, results, collation) = row.unwrap_or_default();
    Ok(tracking::ClientCharset {
        client: client.unwrap_or_default(),
        connection: connection.unwrap_or_default(),
        results: results.unwrap_or_default(),
        collation: collation.unwrap_or_default(),
    })
}

fn setup_error(error: &MyError) -> DriverError {
    let mut error = session_error(error);
    error.message = format!("session setup failed: {}", error.message);
    error
}

/// Sets up session state tracking: the server's tracked variables plus
/// [`tracking::ALWAYS_TRACKED`] and the access mode variable, and (in
/// read-only sessions) the transaction characteristics. Returns the
/// statements it ran, to run again when the tracking has to be restored,
/// or `None` when the server has no session tracking.
async fn tracking_statements(
    conn: &mut Conn,
    read_only_variable: Option<&'static str>,
) -> Result<Option<Vec<String>>, DriverError> {
    let current = match conn
        .query_first::<Option<String>, _>("SELECT @@session.session_track_system_variables")
        .await
    {
        Ok(value) => value.flatten().unwrap_or_default(),
        Err(MyError::Server(error)) if error.code == ER_UNKNOWN_SYSTEM_VARIABLE => {
            return Ok(None);
        }
        Err(error) => return Err(setup_error(&error)),
    };
    let mut wanted: Vec<&str> = tracking::ALWAYS_TRACKED.to_vec();
    wanted.extend(read_only_variable);
    let list = tracking::tracking_list(&current, &wanted).unwrap_or(current);
    let mut statements = vec![format!(
        "SET SESSION session_track_system_variables = '{}'",
        list.replace('\\', "\\\\").replace('\'', "''")
    )];
    if read_only_variable.is_some() {
        statements
            .push("SET SESSION session_track_transaction_info = 'CHARACTERISTICS'".to_owned());
    }
    for statement in &statements {
        conn.query_drop(statement.as_str())
            .await
            .map_err(|error| setup_error(&error))?;
    }
    Ok(Some(statements))
}

/// Finds the access mode variable this server has and reads it.
async fn read_only_state(conn: &mut Conn) -> Result<(&'static str, bool), DriverError> {
    for variable in ["transaction_read_only", "tx_read_only"] {
        let query = format!("SELECT @@session.{variable}");
        match conn.query_first::<Option<i64>, _>(query).await {
            Ok(value) => return Ok((variable, value.flatten() == Some(1))),
            Err(MyError::Server(error)) if error.code == ER_UNKNOWN_SYSTEM_VARIABLE => {}
            Err(error) => return Err(session_error(&error)),
        }
    }
    Err(DriverError::new(
        ErrorKind::Unsupported,
        "the server has neither transaction_read_only nor tx_read_only, so read-only mode \
         cannot be checked",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_engines() {
        assert_eq!(
            detect_engine("10.6.28-MariaDB-ubu2204", "mariadb.org binary distribution"),
            Engine::Mariadb
        );
        assert_eq!(
            detect_engine("8.0.46", "MySQL Community Server - GPL"),
            Engine::Mysql
        );
        // Some MariaDB builds hide the suffix but keep the comment.
        assert_eq!(detect_engine("11.4.4", "MariaDB Server"), Engine::Mariadb);
    }

    #[test]
    fn setup_follows_the_profile_and_engine() {
        let mut params = ConnectParams::new(Engine::Mysql, "db", 3306);
        assert!(setup_statements(&params, Engine::Mysql).is_empty());
        params.read_only = true;
        params.statement_timeout = Some(Duration::from_millis(1_500));
        assert_eq!(
            setup_statements(&params, Engine::Mysql),
            [
                "SET SESSION TRANSACTION READ ONLY",
                "SET SESSION max_execution_time = 1500",
            ]
        );
        assert_eq!(
            setup_statements(&params, Engine::Mariadb)[1],
            "SET SESSION max_statement_time = 1.500"
        );
        // The time zone is not part of it: it runs after `init_sql`.
        assert!(
            !setup_statements(&params, Engine::Mysql)
                .iter()
                .any(|statement| statement.contains("time_zone"))
        );
    }

    #[test]
    fn verify_ca_name_is_a_valid_dns_name() {
        assert!(rustls::pki_types::ServerName::try_from(VERIFY_CA_SERVER_NAME).is_ok());
    }

    #[tokio::test]
    async fn tls_options_follow_the_mode() -> Result<(), DriverError> {
        let mut params = ConnectParams::new(Engine::Mysql, "db.example.com", 3306);
        params.tls.mode = TlsMode::Disable;
        assert!(ssl_opts(&params).await?.is_none());

        params.tls.mode = TlsMode::Require;
        let ssl = ssl_opts(&params)
            .await?
            .ok_or_else(|| DriverError::new(ErrorKind::Tls, "none"))?;
        assert!(ssl.accept_invalid_certs());

        params.tls.mode = TlsMode::VerifyFull;
        params.tls.server_name = Some("real.example.com".into());
        let ssl = ssl_opts(&params)
            .await?
            .ok_or_else(|| DriverError::new(ErrorKind::Tls, "none"))?;
        assert!(!ssl.accept_invalid_certs());
        assert!(!ssl.skip_domain_validation());
        assert_eq!(ssl.tls_hostname_override(), Some("real.example.com"));

        params.tls.mode = TlsMode::VerifyCa;
        let ssl = ssl_opts(&params)
            .await?
            .ok_or_else(|| DriverError::new(ErrorKind::Tls, "none"))?;
        assert!(!ssl.accept_invalid_certs());
        assert_eq!(ssl.tls_hostname_override(), Some(VERIFY_CA_SERVER_NAME));

        params.tls.ca_path = Some("/definitely/not/here.pem".into());
        let error = ssl_opts(&params).await.err().map(|error| error.kind);
        assert_eq!(error, Some(ErrorKind::Tls));

        params.tls.ca_path = None;
        params.tls.client_cert_path = Some("/definitely/not/here.pem".into());
        let error = ssl_opts(&params).await.err().map(|error| error.kind);
        assert_eq!(error, Some(ErrorKind::Tls));
        Ok(())
    }
}
