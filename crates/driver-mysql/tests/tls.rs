//! `VerifyCa` and `VerifyFull` against servers whose CA the test controls.
//!
//! The test generates a CA and a server certificate for `db.varve.invalid`
//! (which does not match `127.0.0.1`, the host it connects to) with rcgen,
//! starts MySQL and MariaDB with them from `tests/tls-servers/compose.yml`
//! and tears them down again. It needs Docker and fails when Docker or the
//! compose setup is missing; run it with
//! `cargo test -p varve-driver-mysql --test tls -- --include-ignored`.
//!
//! Environment: `VARVE_MYSQL_TLS_PROJECT` (compose project, default
//! `varve-mysql-tls`), `VARVE_MYSQL_TLS_MYSQL_PORT` (33190) and
//! `VARVE_MYSQL_TLS_MARIADB_PORT` (33191).

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use varve_core::Engine;
use varve_driver::{
    ConnectParams, ErrorKind, ExecOptions, Session, StatementResult, TlsMode, Value,
};
use varve_driver_mysql::{MysqlDriver, MysqlSession};
use varve_secrets::Secret;

type TestResult = Result<(), Box<dyn Error>>;

/// The only name in the server certificate.
const SERVER_NAME: &str = "db.varve.invalid";

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// Certificates written to a fresh temporary directory.
struct Certificates {
    dir: PathBuf,
}

impl Certificates {
    fn ca(&self) -> String {
        self.dir.join("ca.pem").display().to_string()
    }

    fn other_ca(&self) -> String {
        self.dir.join("other-ca.pem").display().to_string()
    }
}

impl Drop for Certificates {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn ca_params(name: &str) -> Result<CertificateParams, Box<dyn Error>> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    Ok(params)
}

fn write(dir: &Path, name: &str, contents: &str) -> Result<(), Box<dyn Error>> {
    let path = dir.join(name);
    std::fs::write(&path, contents)?;
    // The server in the container runs as another user; these are
    // throwaway test keys.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

/// A CA, a server certificate for [`SERVER_NAME`] signed by it, and an
/// unrelated CA.
fn generate() -> Result<Certificates, Box<dyn Error>> {
    // Cargo's per-target temporary directory lies inside the workspace,
    // which Docker Desktop shares by default (the system temp directory is
    // not always shared).
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("varve-mysql-tls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    }
    let certificates = Certificates { dir };

    let ca_key = KeyPair::generate()?;
    let ca = ca_params("Varve MySQL Test CA")?;
    let ca_cert = ca.self_signed(&ca_key)?;
    let issuer = Issuer::new(ca, ca_key);

    let server_key = KeyPair::generate()?;
    let mut server = CertificateParams::new(vec![SERVER_NAME.to_owned()])?;
    server
        .distinguished_name
        .push(DnType::CommonName, SERVER_NAME);
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server.signed_by(&server_key, &issuer)?;

    let other_key = KeyPair::generate()?;
    let other = ca_params("Varve MySQL Other CA")?.self_signed(&other_key)?;

    let dir = &certificates.dir;
    write(dir, "ca.pem", &ca_cert.pem())?;
    write(dir, "server-cert.pem", &server_cert.pem())?;
    write(dir, "server-key.pem", &server_key.serialize_pem())?;
    write(dir, "other-ca.pem", &other.pem())?;
    Ok(certificates)
}

/// The TLS servers; stopped and removed when dropped.
struct Servers {
    project: String,
    compose: PathBuf,
    dir: PathBuf,
}

impl Servers {
    fn compose(&self, args: &[&str]) -> Result<(), Box<dyn Error>> {
        self.compose_output(args).map(|_| ())
    }

    fn compose_output(&self, args: &[&str]) -> Result<String, Box<dyn Error>> {
        let output = Command::new("docker")
            .arg("compose")
            .args(["-p", &self.project, "-f"])
            .arg(&self.compose)
            .args(args)
            .env("VARVE_MYSQL_TLS_DIR", &self.dir)
            .output()
            .map_err(|error| format!("cannot run docker compose: {error}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "docker compose {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            )
            .into())
        }
    }

    fn start(certificates: &Certificates) -> Result<Self, Box<dyn Error>> {
        let compose = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tls-servers/compose.yml");
        if !compose.is_file() {
            return Err(format!("{} is missing", compose.display()).into());
        }
        let servers = Self {
            project: env_or("VARVE_MYSQL_TLS_PROJECT", "varve-mysql-tls"),
            compose,
            dir: certificates.dir.clone(),
        };
        // Leftovers of an aborted run would keep old certificates.
        servers.compose(&["down", "-v"])?;
        if let Err(error) = servers.compose(&["up", "-d", "--wait"]) {
            let logs = servers
                .compose_output(&["logs", "--tail", "40"])
                .unwrap_or_else(|logs_error| logs_error.to_string());
            return Err(format!("{error}\n{logs}").into());
        }
        Ok(servers)
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        if let Err(error) = self.compose(&["down", "-v"]) {
            eprintln!("{error}");
        }
    }
}

fn params(engine: Engine, port: u16) -> ConnectParams {
    let mut params = ConnectParams::new(engine, "127.0.0.1", port);
    params.username = Some("varve".into());
    params.password = Some(Secret::new("varve_dev".into()));
    params.database = Some("varve".into());
    params.application_name = "varve-tests".into();
    params
}

fn check(name: &str, ok: bool, what: impl std::fmt::Display) -> TestResult {
    if ok {
        Ok(())
    } else {
        Err(format!("{name}: {what}").into())
    }
}

async fn open(name: &str, params: &ConnectParams) -> Result<MysqlSession, Box<dyn Error>> {
    MysqlDriver::new()
        .open(params)
        .await
        .map_err(|error| format!("{name}: {:?}: {error:?}", params.tls.mode).into())
}

async fn refused(name: &str, params: &ConnectParams, reason: &str) -> TestResult {
    match MysqlDriver::new().open(params).await {
        Ok(_) => Err(format!("{name}: {:?} connected, expected {reason}", params.tls.mode).into()),
        Err(error) => check(
            name,
            error.kind == ErrorKind::Tls && error.message.contains(reason),
            format!("{:?}: {error:?}", params.tls.mode),
        ),
    }
}

async fn cipher(session: &MysqlSession) -> Result<Value, Box<dyn Error>> {
    let output = session
        .execute(
            "SHOW SESSION STATUS LIKE 'Ssl_cipher'",
            &ExecOptions::default(),
        )
        .await?;
    match output.results.first() {
        Some(StatementResult::Rows { rows, .. }) => Ok(rows
            .first()
            .and_then(|row| row.get(1))
            .cloned()
            .unwrap_or(Value::Null)),
        other => Err(format!("unexpected {other:?}").into()),
    }
}

async fn verify_modes(name: &str, engine: Engine, port: u16, certs: &Certificates) -> TestResult {
    let mut params = params(engine, port);
    params.tls.ca_path = Some(certs.ca());

    // VerifyCa: the chain is trusted and the name is not checked.
    params.tls.mode = TlsMode::VerifyCa;
    let session = Arc::new(open(name, &params).await?);
    check(name, session.uses_tls(), "VerifyCa without TLS")?;
    let cipher = cipher(&session).await?;
    check(
        name,
        matches!(&cipher, Value::Text(text) if !text.is_empty()),
        format!("Ssl_cipher {cipher:?}"),
    )?;
    // Cancel opens its kill connection with the same TLS options.
    let running = Arc::clone(&session);
    let task = tokio::spawn(async move {
        running
            .execute(
                // Runs for minutes and fails with error 1317 when killed
                // (a killed SLEEP() ends without an error on MySQL).
                "SELECT COUNT(*) FROM information_schema.columns a, \
                 information_schema.columns b, information_schema.columns c",
                &ExecOptions::default(),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    session.cancel_handle().cancel().await?;
    let outcome = tokio::time::timeout(Duration::from_secs(3), task).await??;
    check(
        name,
        matches!(&outcome, Err(error) if error.kind == ErrorKind::Cancelled),
        format!("cancel under VerifyCa: {outcome:?}"),
    )?;

    // VerifyFull: the certificate is not valid for 127.0.0.1.
    params.tls.mode = TlsMode::VerifyFull;
    refused(name, &params, "not valid for name").await?;
    // With the certificate's own name, VerifyFull passes.
    params.tls.server_name = Some(SERVER_NAME.into());
    let session = open(name, &params).await?;
    check(name, session.uses_tls(), "VerifyFull without TLS")?;
    params.tls.server_name = None;

    // A certificate from another CA is refused under both modes.
    params.tls.ca_path = Some(certs.other_ca());
    for mode in [TlsMode::VerifyCa, TlsMode::VerifyFull] {
        params.tls.mode = mode;
        refused(name, &params, "UnknownIssuer").await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker: starts its own TLS servers"]
async fn verify_ca_accepts_a_trusted_certificate_for_another_name() -> TestResult {
    let port = |name: &str, default: u16| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    let certificates = generate()?;
    let servers = Servers::start(&certificates)?;
    let outcome = async {
        verify_modes(
            "MySQL 8.0",
            Engine::Mysql,
            port("VARVE_MYSQL_TLS_MYSQL_PORT", 33190),
            &certificates,
        )
        .await?;
        verify_modes(
            "MariaDB 12.3",
            Engine::Mariadb,
            port("VARVE_MYSQL_TLS_MARIADB_PORT", 33191),
            &certificates,
        )
        .await
    }
    .await;
    drop(servers);
    outcome
}
