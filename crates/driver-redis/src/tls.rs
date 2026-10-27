//! rustls client configuration for the TLS modes of a connection profile.

use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
    ServerName, UnixTime,
};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use varve_driver::tls_key;
use varve_driver::{DriverError, TlsMode, TlsParams};

use crate::error;

/// What the connector does with TLS.
#[derive(Clone)]
pub enum TlsPlan {
    /// Plain TCP.
    Plain,
    /// TLS only.
    Tls(TlsTarget),
    /// TLS first, plain TCP when the TLS handshake fails.
    TlsThenPlain(TlsTarget),
}

/// A ready TLS configuration and the name to present and check.
#[derive(Clone)]
pub struct TlsTarget {
    /// rustls configuration.
    pub config: Arc<ClientConfig>,
    /// SNI and certificate name.
    pub server_name: ServerName<'static>,
}

/// Builds the TLS plan for `params`, reading CA and client identity files
/// (an encrypted client key is decrypted with its passphrase, see
/// `varve_driver::tls_key`).
///
/// # Errors
///
/// [`varve_driver::ErrorKind::Tls`] when a file cannot be read or parsed,
/// when only one of client certificate and key is set, when the client key
/// needs a passphrase that is missing or wrong (with the codes of
/// `varve_driver::tls_key`), or when the server name is not a valid DNS name
/// or IP address.
pub async fn plan(
    params: &TlsParams,
    host: &str,
    timeout: std::time::Duration,
) -> Result<TlsPlan, DriverError> {
    if params.mode == TlsMode::Disable {
        return Ok(TlsPlan::Plain);
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let name = params.server_name.as_deref().unwrap_or(host).to_string();
    let server_name = ServerName::try_from(name.clone())
        .map_err(|_| error::tls(format!("{name} is not a valid server name")))?;

    let verifier: Arc<dyn ServerCertVerifier> = match params.mode {
        TlsMode::Disable | TlsMode::Prefer | TlsMode::Require => Arc::new(NoVerification {
            algorithms: provider.signature_verification_algorithms,
        }),
        TlsMode::VerifyCa | TlsMode::VerifyFull => {
            let roots = Arc::new(root_store(params.ca_path.as_deref())?);
            let webpki = WebPkiServerVerifier::builder_with_provider(roots, provider.clone())
                .build()
                .map_err(error::tls)?;
            if params.mode == TlsMode::VerifyCa {
                Arc::new(IgnoreHostname { inner: webpki })
            } else {
                webpki
            }
        }
    };

    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(error::tls)?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let config = match tls_key::load_client_identity(params, timeout).await? {
        Some(identity) => {
            let chain = read_certs(&identity.cert_path)?;
            builder
                .with_client_auth_cert(chain, rustls_key(&identity.key))
                .map_err(error::tls)?
        }
        None => builder.with_no_client_auth(),
    };
    let target = TlsTarget {
        config: Arc::new(config),
        server_name,
    };
    Ok(if params.mode == TlsMode::Prefer {
        TlsPlan::TlsThenPlain(target)
    } else {
        TlsPlan::Tls(target)
    })
}

/// The key in the form rustls takes.
fn rustls_key(key: &tls_key::PrivateKeyDer) -> PrivateKeyDer<'static> {
    match key {
        tls_key::PrivateKeyDer::Pkcs8(der) => PrivatePkcs8KeyDer::from(der.to_vec()).into(),
        tls_key::PrivateKeyDer::Pkcs1(der) => PrivatePkcs1KeyDer::from(der.to_vec()).into(),
        tls_key::PrivateKeyDer::Sec1(der) => PrivateSec1KeyDer::from(der.to_vec()).into(),
    }
}

fn cert_or_key_path(path: &str) -> &std::path::Path {
    std::path::Path::new(path)
}

fn read_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, DriverError> {
    let certs = CertificateDer::pem_file_iter(cert_or_key_path(path))
        .map_err(|e| error::tls(format!("cannot read {path}: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| error::tls(format!("cannot parse {path}: {e}")))?;
    if certs.is_empty() {
        return Err(error::tls(format!("{path} holds no certificate")));
    }
    Ok(certs)
}

fn root_store(ca_path: Option<&str>) -> Result<RootCertStore, DriverError> {
    let mut roots = RootCertStore::empty();
    match ca_path {
        Some(path) => {
            for cert in read_certs(path)? {
                roots
                    .add(cert)
                    .map_err(|e| error::tls(format!("bad CA certificate in {path}: {e}")))?;
            }
        }
        None => {
            let native = rustls_native_certs::load_native_certs();
            for problem in &native.errors {
                tracing::warn!(%problem, "could not load an OS trust store certificate");
            }
            let (added, _ignored) = roots.add_parsable_certificates(native.certs);
            if added == 0 {
                return Err(error::tls("the OS trust store holds no usable certificate"));
            }
        }
    }
    Ok(roots)
}

/// Accepts any certificate: used for `prefer` and `require`, which encrypt
/// without authenticating the server. Handshake signatures are still checked
/// so the session keys belong to the presented certificate.
#[derive(Debug)]
struct NoVerification {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Checks the chain against the roots but accepts any host name
/// (`verify_ca`).
#[derive(Debug)]
struct IgnoreHostname {
    inner: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for IgnoreHostname {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName
                | rustls::CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use varve_driver::ErrorKind;

    use super::*;

    const LONG: std::time::Duration = std::time::Duration::from_secs(60);

    fn params(mode: TlsMode) -> TlsParams {
        TlsParams {
            mode,
            ..TlsParams::default()
        }
    }

    #[tokio::test]
    async fn modes_map_to_plans() -> Result<(), DriverError> {
        assert!(matches!(
            plan(&params(TlsMode::Disable), "localhost", LONG).await?,
            TlsPlan::Plain
        ));
        assert!(matches!(
            plan(&params(TlsMode::Prefer), "localhost", LONG).await?,
            TlsPlan::TlsThenPlain(_)
        ));
        assert!(matches!(
            plan(&params(TlsMode::Require), "127.0.0.1", LONG).await?,
            TlsPlan::Tls(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn server_name_override_is_used() -> Result<(), DriverError> {
        let mut p = params(TlsMode::Require);
        p.server_name = Some("redis.internal".into());
        match plan(&p, "127.0.0.1", LONG).await? {
            TlsPlan::Tls(target) => assert_eq!(
                target.server_name,
                ServerName::try_from("redis.internal").map_err(error::tls)?
            ),
            _ => panic!("expected TLS"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn missing_files_are_tls_errors() {
        let mut p = params(TlsMode::VerifyFull);
        p.ca_path = Some("/nonexistent/ca.pem".into());
        assert_eq!(
            plan(&p, "localhost", LONG).await.err().map(|e| e.kind),
            Some(ErrorKind::Tls)
        );

        let mut p = params(TlsMode::Require);
        p.client_cert_path = Some("/nonexistent/cert.pem".into());
        let error = plan(&p, "localhost", LONG).await.err();
        assert_eq!(error.as_ref().map(|e| e.kind), Some(ErrorKind::Tls));
        assert!(error.is_some_and(|e| e.message.contains("without its private key")));

        let mut p = params(TlsMode::Require);
        p.client_cert_path = Some("/nonexistent/cert.pem".into());
        p.client_key_path = Some("/nonexistent/key.pem".into());
        assert_eq!(
            plan(&p, "localhost", LONG).await.err().map(|e| e.kind),
            Some(ErrorKind::Tls)
        );
    }

    #[tokio::test]
    async fn bad_server_name_is_rejected() {
        assert_eq!(
            plan(&params(TlsMode::Require), "not a host!", LONG)
                .await
                .err()
                .map(|e| e.kind),
            Some(ErrorKind::Tls)
        );
    }
}
