//! A server that asks for the password in clear text
//! (AuthenticationCleartextPassword, pg_hba `password`, `ldap`, `radius`,
//! `pam`) on a connection without TLS gets it as is: the session reports
//! that through `Session::password_in_clear`, so the app can show it.
//!
//! Starts `tests/cleartext/compose.yml` (PostgreSQL 18 without TLS, one
//! server with the `password` method, one with the default SCRAM) under its
//! own compose project (`VARVE_PG_CLEARTEXT_PROJECT`, default
//! `varve-pg-cleartext-<process id>`; Docker picks the host ports) and tears
//! it down again. It needs Docker; run it with
//! `cargo test -p varve-driver-postgres --test cleartext_password -- --include-ignored`.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, TlsMode};
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type TestResult = Result<(), Box<dyn Error>>;

/// The servers; stopped and removed when dropped.
struct Servers {
    project: String,
    compose: PathBuf,
}

impl Servers {
    fn compose(&self, args: &[&str]) -> Result<String, Box<dyn Error>> {
        let output = Command::new("docker")
            .arg("compose")
            .args(["-p", &self.project, "-f"])
            .arg(&self.compose)
            .args(args)
            .output()
            .map_err(|error| {
                format!("cannot run docker compose (this test needs Docker): {error}")
            })?;
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

    fn start() -> Result<Self, Box<dyn Error>> {
        let compose = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cleartext/compose.yml");
        if !compose.is_file() {
            return Err(format!("{} is missing", compose.display()).into());
        }
        let servers = Self {
            project: std::env::var("VARVE_PG_CLEARTEXT_PROJECT")
                .unwrap_or_else(|_| format!("varve-pg-cleartext-{}", std::process::id())),
            compose,
        };
        servers.compose(&["down", "-v"])?;
        if let Err(error) = servers.compose(&["up", "-d", "--wait"]) {
            let logs = servers
                .compose(&["logs", "--tail", "40"])
                .unwrap_or_else(|logs_error| logs_error.to_string());
            return Err(format!(
                "could not start the PostgreSQL fixture (compose project {}, needs a running \
                 Docker): {error}\n{logs}",
                servers.project
            )
            .into());
        }
        Ok(servers)
    }

    /// The host port Docker picked for `service`.
    fn port(&self, service: &str) -> Result<u16, Box<dyn Error>> {
        let output = self.compose(&["port", service, "5432"])?;
        output
            .trim()
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse().ok())
            .ok_or_else(|| format!("no host port for {service}: {output:?}").into())
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        if let Err(error) = self.compose(&["down", "-v"]) {
            eprintln!("{error}");
        }
    }
}

fn params(port: u16, mode: TlsMode) -> ConnectParams {
    let mut params = ConnectParams::new(Engine::Postgres, "127.0.0.1", port);
    params.username = Some("varve".into());
    params.password = Some(Secret::new("varve_dev".into()));
    params.database = Some("varve".into());
    params.tls.mode = mode;
    params
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker for tests/cleartext/compose.yml"]
async fn a_cleartext_password_request_without_tls_is_reported() -> TestResult {
    let servers = Servers::start()?;
    let driver = PostgresDriver::new();

    // The `password` method: the password goes as is, over plain TCP
    // (`prefer` against a server without TLS, and `disable`).
    let port = servers.port("password")?;
    for mode in [TlsMode::Prefer, TlsMode::Disable] {
        let session = driver.connect(&params(port, mode)).await?;
        assert_eq!(session.tls_in_use(), Some(false), "{mode:?}");
        assert!(session.password_in_clear(), "{mode:?}");
        session.close().await?;
    }

    // SCRAM never sends the password itself.
    let port = servers.port("scram")?;
    let session = driver.connect(&params(port, TlsMode::Prefer)).await?;
    assert_eq!(session.tls_in_use(), Some(false));
    assert!(!session.password_in_clear());
    session.close().await?;
    Ok(())
}
