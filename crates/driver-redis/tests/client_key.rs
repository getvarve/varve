//! Client certificate login with an encrypted client key (design doc 3.1).
//!
//! The test generates a CA, server and client certificates with rcgen and
//! encrypts the client key (PKCS#8, scrypt and PBKDF2) with the pkcs8 crate,
//! then starts Redis 6.2 and 8.10 from `tests/client-key/compose.yml` on a
//! TLS port with `tls-auth-clients yes`, so every client needs a certificate
//! signed by the CA. The compose project is torn down at the end. Run it
//! with `cargo test -p varve-driver-redis --test client_key -- --include-ignored`
//! (needs Docker; `VARVE_TLSKEY_PROJECT` overrides the compose project).

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

use pkcs8::LineEnding;
use pkcs8::pkcs5::{pbes2, scrypt};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use varve_driver::tls_key::{PASSPHRASE_REQUIRED, PASSPHRASE_WRONG};
use varve_driver::{ConnectParams, ErrorKind, Session};
use varve_driver::{ExecOptions, TlsMode};
use varve_secrets::Secret;

type TestResult = Result<(), Box<dyn Error>>;

/// Passphrase of the encrypted client keys (throwaway test keys).
const PASSPHRASE: &str = "Client-Key-Pass";

/// A wrong passphrase; it must never show up in an error.
const WRONG: &str = "Wrong-Pass-4711";

/// Certificates in a fresh directory and the compose project started with
/// them; both are removed when dropped.
struct Fixture {
    dir: PathBuf,
    compose: PathBuf,
    project: String,
}

impl Fixture {
    /// Writes a CA, a server certificate for `localhost` and `127.0.0.1`, a
    /// client certificate for `client_name` with its key plain, encrypted
    /// with scrypt and with PBKDF2, plus `extra` files, then starts the
    /// compose file.
    fn start(client_name: &str, extra: &[(&str, &str)]) -> Result<Self, Box<dyn Error>> {
        // Cargo's per-target temporary directory lies inside the workspace,
        // which Docker shares (the system temp directory is not always).
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("{PROJECT}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        set_mode(&dir, 0o755)?;
        let fixture = Self {
            dir,
            compose: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/client-key/compose.yml"),
            project: std::env::var("VARVE_TLSKEY_PROJECT")
                .unwrap_or_else(|_| format!("{PROJECT}-{}", std::process::id())),
        };

        let ca_key = KeyPair::generate()?;
        let mut ca = CertificateParams::new(Vec::<String>::new())?;
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "Varve client key test CA");
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_cert = ca.self_signed(&ca_key)?;
        let issuer = Issuer::new(ca, ca_key);

        let server_key = KeyPair::generate()?;
        let mut server =
            CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
        server
            .distinguished_name
            .push(DnType::CommonName, "localhost");
        server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_cert = server.signed_by(&server_key, &issuer)?;

        let client_key = KeyPair::generate()?;
        let mut client = CertificateParams::new(Vec::<String>::new())?;
        client
            .distinguished_name
            .push(DnType::CommonName, client_name);
        client.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client_cert = client.signed_by(&client_key, &issuer)?;

        let scrypt = pbes2::Parameters::generate_scrypt_aes256cbc(
            scrypt::Params::new(14, 8, 1).map_err(|e| e.to_string())?,
            &[7; 16],
            [9; 16],
        )?;
        let pbkdf2 = pbes2::Parameters::generate_pbkdf2_sha256_aes256cbc(2048, &[5; 16], [3; 16])?;

        let files = [
            ("ca.pem", ca_cert.pem()),
            ("server-cert.pem", server_cert.pem()),
            ("server-key.pem", server_key.serialize_pem()),
            ("client-cert.pem", client_cert.pem()),
            ("client-key.pem", client_key.serialize_pem()),
            ("client-scrypt.pem", encrypt(&client_key, scrypt)?),
            ("client-pbkdf2.pem", encrypt(&client_key, pbkdf2)?),
        ];
        for (name, contents) in &files {
            fixture.write(name, contents)?;
        }
        for (name, contents) in extra {
            fixture.write(name, contents)?;
        }

        // Leftovers of an aborted run would keep old certificates.
        fixture.compose(&["down", "-v"])?;
        if let Err(error) = fixture.compose(&["up", "-d", "--wait"]) {
            let logs = fixture
                .compose(&["logs", "--tail", "60"])
                .unwrap_or_else(|logs_error| logs_error.to_string());
            return Err(format!(
                "could not start compose project {} (needs a running Docker): {error}\n{logs}",
                fixture.project
            )
            .into());
        }
        Ok(fixture)
    }

    fn write(&self, name: &str, contents: &str) -> TestResult {
        let path = self.dir.join(name);
        std::fs::write(&path, contents)?;
        // The servers in the containers run as other users.
        set_mode(&path, 0o644)
    }

    fn path(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }

    fn compose(&self, args: &[&str]) -> Result<String, Box<dyn Error>> {
        let output = Command::new("docker")
            .arg("compose")
            .args(["-p", &self.project, "-f"])
            .arg(&self.compose)
            .args(args)
            .env("VARVE_TLSKEY_DIR", &self.dir)
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

    /// The host port Docker picked for `service`.
    fn port(&self, service: &str, container_port: u16) -> Result<u16, Box<dyn Error>> {
        let output = self.compose(&["port", service, &container_port.to_string()])?;
        output
            .trim()
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse().ok())
            .ok_or_else(|| format!("no host port for {service}: {output:?}").into())
    }

    /// `base` with the client certificate and the key file `key`.
    fn with_key(&self, base: &ConnectParams, key: &str, passphrase: Option<&str>) -> ConnectParams {
        let mut params = base.clone();
        params.tls.client_cert_path = Some(self.path("client-cert.pem"));
        params.tls.client_key_path = Some(self.path(key));
        params.tls.client_key_passphrase = passphrase.map(|p| Secret::new(p.to_owned()));
        params
    }

    /// The cases every engine shares: both encrypted keys log in with the
    /// passphrase, the plain key without one (a given one is ignored), a
    /// missing or wrong passphrase fails with its code before the server is
    /// asked, and the server refuses a login without a client certificate.
    /// Returns a session per encrypted key.
    async fn key_cases(
        &self,
        label: &str,
        base: &ConnectParams,
    ) -> Result<Vec<Box<dyn Session>>, Box<dyn Error>> {
        let mut sessions = Vec::new();
        for key in ["client-scrypt.pem", "client-pbkdf2.pem"] {
            let session = connect(&self.with_key(base, key, Some(PASSPHRASE)))
                .await
                .map_err(|e| format!("{label} {key}: {e:?}"))?;
            session.ping().await?;
            sessions.push(session);
        }
        connect(&self.with_key(base, "client-key.pem", Some("unused")))
            .await
            .map_err(|e| format!("{label} plain key: {e:?}"))?
            .close()
            .await?;

        let missing = connect(&self.with_key(base, "client-scrypt.pem", None))
            .await
            .err()
            .ok_or(format!("{label}: connected without the passphrase"))?;
        assert_eq!(missing.kind, ErrorKind::Tls, "{label}: {missing:?}");
        assert_eq!(
            missing.code.as_deref(),
            Some(PASSPHRASE_REQUIRED),
            "{label}"
        );

        for key in ["client-scrypt.pem", "client-pbkdf2.pem"] {
            let wrong = connect(&self.with_key(base, key, Some(WRONG)))
                .await
                .err()
                .ok_or(format!("{label} {key}: connected with a wrong passphrase"))?;
            assert_eq!(wrong.kind, ErrorKind::Tls, "{label} {key}: {wrong:?}");
            assert_eq!(
                wrong.code.as_deref(),
                Some(PASSPHRASE_WRONG),
                "{label} {key}"
            );
            let text = format!("{wrong:?} {wrong}");
            assert!(
                !text.contains(WRONG),
                "{label}: the passphrase leaked: {text}"
            );
        }

        let mut no_certificate = base.clone();
        no_certificate.tls.client_cert_path = None;
        no_certificate.tls.client_key_path = None;
        let refused = connect(&no_certificate)
            .await
            .err()
            .ok_or(format!("{label}: logged in without a client certificate"))?;
        assert_ne!(
            refused.code.as_deref(),
            Some(PASSPHRASE_REQUIRED),
            "{label}"
        );
        Ok(sessions)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = self.compose(&["down", "-v"]) {
            eprintln!("{error}");
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The client key as `ENCRYPTED PRIVATE KEY` PEM (PKCS#8, PBES2).
fn encrypt(key: &KeyPair, parameters: pbes2::Parameters) -> Result<String, Box<dyn Error>> {
    let der = key.serialize_der();
    let info = pkcs8::PrivateKeyInfoRef::try_from(der.as_slice())?;
    let document = info.encrypt_with_params(parameters, PASSPHRASE)?;
    Ok(document
        .to_pem("ENCRYPTED PRIVATE KEY", LineEnding::LF)?
        .to_string())
}

fn set_mode(path: &Path, mode: u32) -> TestResult {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// Compose project prefix (plus the process id).
const PROJECT: &str = "varve-tlskey-redis";

async fn connect(params: &ConnectParams) -> Result<Box<dyn Session>, varve_driver::DriverError> {
    use varve_driver::Driver;
    varve_driver_redis::RedisDriver::new().connect(params).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker for the client key fixture"]
async fn encrypted_client_keys_pass_tls_auth_clients() -> TestResult {
    let fixture = Fixture::start("varve-redis-client", &[])?;
    for service in ["redis-6.2", "redis-8.10"] {
        let mut base = ConnectParams::new(
            varve_core::Engine::Redis,
            "127.0.0.1",
            fixture.port(service, 6379)?,
        );
        base.password = Some(Secret::new("varve_dev".into()));
        base.tls.mode = TlsMode::VerifyFull;
        base.tls.ca_path = Some(fixture.path("ca.pem"));
        let sessions = fixture.key_cases(service, &base).await?;
        for session in sessions {
            assert_eq!(session.tls_in_use(), Some(true), "{service}");
            let output = session
                .execute("SET varve:tlskey ok", &ExecOptions::default())
                .await?;
            assert!(!output.results.is_empty(), "{service}");
            session.close().await?;
        }
    }
    Ok(())
}
