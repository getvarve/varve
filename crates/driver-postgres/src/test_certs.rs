//! Test certificates, generated with rcgen when needed so no private key is
//! ever committed. Used by the TLS unit tests and by the
//! `tls-test-certs` example, which writes them for the TLS test server.

// The unit tests and the example each use part of this module.
#![allow(dead_code)]

use std::path::Path;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};

/// PostgreSQL role the client certificate is issued to (its common name).
pub const CLIENT_USER: &str = "varve_cert";

/// A test CA with a server and a client certificate, plus an unrelated CA.
pub struct TestPki {
    /// The CA that signed the server and client certificates.
    pub ca_cert: String,
    /// Server certificate for `localhost` and `127.0.0.1`.
    pub server_cert: String,
    /// The server certificate's private key (PKCS#8 PEM).
    pub server_key: String,
    /// Client certificate for [`CLIENT_USER`].
    pub client_cert: String,
    /// The client certificate's private key (PKCS#8 PEM).
    pub client_key: String,
    /// A CA that signed nothing the server uses.
    pub other_ca_cert: String,
}

fn ca(name: &str) -> Result<(String, Issuer<'static, KeyPair>), rcgen::Error> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), Issuer::new(params, key)))
}

fn leaf(
    names: &[&str],
    common_name: &str,
    usage: ExtendedKeyUsagePurpose,
    issuer: &Issuer<'_, KeyPair>,
) -> Result<(String, String), rcgen::Error> {
    let mut params =
        CertificateParams::new(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>())?;
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![usage];
    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, issuer)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Generates a fresh set of certificates.
pub fn generate() -> Result<TestPki, rcgen::Error> {
    let (ca_cert, issuer) = ca("Varve Test CA")?;
    let (server_cert, server_key) = leaf(
        &["localhost", "127.0.0.1"],
        "localhost",
        ExtendedKeyUsagePurpose::ServerAuth,
        &issuer,
    )?;
    let (client_cert, client_key) = leaf(
        &[],
        CLIENT_USER,
        ExtendedKeyUsagePurpose::ClientAuth,
        &issuer,
    )?;
    let (other_ca_cert, _) = ca("Varve Unrelated Test CA")?;
    Ok(TestPki {
        ca_cert,
        server_cert,
        server_key,
        client_cert,
        client_key,
        other_ca_cert,
    })
}

impl TestPki {
    /// Writes `ca.crt`, `server.crt`, `server.key`, `client.crt`,
    /// `client.key` and `other-ca.crt` into `dir`.
    pub fn write_to(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        for (name, pem) in [
            ("ca.crt", &self.ca_cert),
            ("server.crt", &self.server_cert),
            ("server.key", &self.server_key),
            ("client.crt", &self.client_cert),
            ("client.key", &self.client_key),
            ("other-ca.crt", &self.other_ca_cert),
        ] {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            std::io::Write::write_all(&mut options.open(dir.join(name))?, pem.as_bytes())?;
        }
        Ok(())
    }
}
