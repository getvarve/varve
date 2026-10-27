//! TLS setup: rustls client configurations for every [`TlsMode`] and the
//! connector that plugs them into tokio-postgres.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{WebPkiServerVerifier, verify_server_cert_signed_by_trust_anchor};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
    ServerName, UnixTime,
};
use rustls::server::ParsedCertificate;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::net::TcpStream;
use tokio_postgres::config::SslMode;
use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};
use tokio_postgres_rustls::MakeRustlsConnect;
use varve_driver::tls_key::{self, ClientIdentity};
use varve_driver::{DriverError, ErrorKind, TlsMode, TlsParams};

use crate::tap::{Shared, TapStream};

/// The raw socket as tokio-postgres sees it.
pub(crate) type RawStream = TapStream<TcpStream>;

type RustlsConnect = <MakeRustlsConnect as MakeTlsConnect<RawStream>>::TlsConnect;
type RustlsStream = <MakeRustlsConnect as MakeTlsConnect<RawStream>>::Stream;

/// Everything needed to open (and later cancel on) a TLS connection.
#[derive(Clone)]
pub(crate) struct TlsSetup {
    /// The mode handed to tokio-postgres.
    pub(crate) ssl_mode: SslMode,
    /// `None` when TLS is disabled.
    connector: Option<MakeRustlsConnect>,
    /// Name sent as SNI and, in `VerifyFull`, checked against the certificate.
    server_name: String,
}

impl TlsSetup {
    /// Builds the TLS setup for a connection to `host`, reading (and
    /// decrypting, within `timeout`) the client key when one is configured.
    pub(crate) async fn new(
        params: &TlsParams,
        host: &str,
        timeout: std::time::Duration,
    ) -> Result<Self, DriverError> {
        let ssl_mode = match params.mode {
            TlsMode::Disable => SslMode::Disable,
            TlsMode::Prefer => SslMode::Prefer,
            TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull => SslMode::Require,
        };
        if params.mode == TlsMode::Disable {
            return Ok(Self {
                ssl_mode,
                connector: None,
                server_name: host.to_owned(),
            });
        }
        let wanted = params.server_name.as_deref().unwrap_or(host).trim();
        let server_name = if ServerName::try_from(wanted).is_ok() {
            wanted.to_owned()
        } else if params.mode == TlsMode::VerifyFull {
            return Err(tls_error(format!(
                "\"{wanted}\" is not a valid host name to check the server certificate against"
            )));
        } else {
            // Only used for SNI when the certificate name is not checked;
            // a name rustls cannot send is left out by using a neutral one.
            "localhost".to_owned()
        };
        let identity = tls_key::load_client_identity(params, timeout).await?;
        let config = client_config(params, identity.as_ref())?;
        Ok(Self {
            ssl_mode,
            connector: Some(MakeRustlsConnect::new(config)),
            server_name,
        })
    }

    /// A connector for one connection attempt. `shared` receives what the
    /// tap of the encrypted stream sees.
    pub(crate) fn connector(&self, shared: Arc<Shared>) -> TapTls {
        let inner = self.connector.clone().map(|mut make| {
            match <MakeRustlsConnect as MakeTlsConnect<RawStream>>::make_tls_connect(
                &mut make,
                &self.server_name,
            ) {
                Ok(connect) => connect,
                Err(never) => match never {},
            }
        });
        TapTls { inner, shared }
    }
}

/// A tokio-postgres TLS connector: tokio-postgres-rustls, with the result
/// wrapped in a [`TapStream`].
pub(crate) struct TapTls {
    inner: Option<RustlsConnect>,
    shared: Arc<Shared>,
}

impl TlsConnect<RawStream> for TapTls {
    type Stream = TapStream<RustlsStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Stream>> + Send>>;

    fn connect(self, mut stream: RawStream) -> Self::Future {
        stream.stop_parsing();
        let shared = self.shared;
        match self.inner {
            Some(inner) => Box::pin(async move {
                let tls = inner.connect(stream).await?;
                Ok(TapStream::parsing(tls, shared))
            }),
            None => {
                Box::pin(async { Err(io::Error::other("TLS is disabled for this connection")) })
            }
        }
    }
}

fn tls_error(message: impl Into<String>) -> DriverError {
    DriverError::new(ErrorKind::Tls, message)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn client_config(
    params: &TlsParams,
    identity: Option<&ClientIdentity>,
) -> Result<ClientConfig, DriverError> {
    let provider = provider();
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_error(format!("TLS setup failed: {e}")))?;
    let builder = match params.mode {
        TlsMode::Disable | TlsMode::Prefer | TlsMode::Require => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateCheck { provider })),
        TlsMode::VerifyCa => {
            let roots = Arc::new(root_store(params.ca_path.as_deref())?);
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(ChainOnlyVerifier { roots, provider }))
        }
        TlsMode::VerifyFull => {
            let roots = Arc::new(root_store(params.ca_path.as_deref())?);
            let verifier = WebPkiServerVerifier::builder_with_provider(roots, provider)
                .build()
                .map_err(|e| tls_error(format!("TLS setup failed: {e}")))?;
            builder.with_webpki_verifier(verifier)
        }
    };
    match identity {
        None => Ok(builder.with_no_client_auth()),
        Some(identity) => {
            let certs = load_certificates(&identity.cert_path, "client certificate")?;
            builder
                .with_client_auth_cert(certs, rustls_key(&identity.key))
                .map_err(|e| tls_error(format!("the client certificate cannot be used: {e}")))
        }
    }
}

/// The key in the form rustls takes.
fn rustls_key(key: &tls_key::PrivateKeyDer) -> PrivateKeyDer<'static> {
    match key {
        tls_key::PrivateKeyDer::Pkcs8(der) => PrivatePkcs8KeyDer::from(der.to_vec()).into(),
        tls_key::PrivateKeyDer::Pkcs1(der) => PrivatePkcs1KeyDer::from(der.to_vec()).into(),
        tls_key::PrivateKeyDer::Sec1(der) => PrivateSec1KeyDer::from(der.to_vec()).into(),
    }
}

fn load_certificates(path: &str, what: &str) -> Result<Vec<CertificateDer<'static>>, DriverError> {
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| tls_error(format!("could not read the {what} from \"{path}\": {e}")))?;
    if certs.is_empty() {
        return Err(tls_error(format!(
            "\"{path}\" contains no PEM certificate for the {what}"
        )));
    }
    Ok(certs)
}

/// Trust anchors: the PEM file at `ca_path`, or the OS trust store.
fn root_store(ca_path: Option<&str>) -> Result<RootCertStore, DriverError> {
    let mut roots = RootCertStore::empty();
    if let Some(path) = ca_path {
        for cert in load_certificates(path, "CA certificate")? {
            roots.add(cert).map_err(|e| {
                tls_error(format!("\"{path}\" holds an unusable CA certificate: {e}"))
            })?;
        }
        return Ok(roots);
    }
    let native = rustls_native_certs::load_native_certs();
    let (added, _ignored) = roots.add_parsable_certificates(native.certs);
    if added == 0 {
        let reasons: Vec<String> = native.errors.iter().map(ToString::to_string).collect();
        return Err(tls_error(if reasons.is_empty() {
            "the OS trust store holds no CA certificates".to_owned()
        } else {
            format!("could not load the OS trust store: {}", reasons.join("; "))
        }));
    }
    Ok(roots)
}

/// `Prefer` and `Require`: any certificate is accepted, but the handshake
/// signatures are still checked so the peer proves it holds the key of the
/// certificate it sent.
#[derive(Debug)]
struct NoCertificateCheck {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for NoCertificateCheck {
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
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `VerifyCa`: the chain must lead to a trusted CA and be valid now; the
/// host name is not checked.
#[derive(Debug)]
struct ChainOnlyVerifier {
    roots: Arc<RootCertStore>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for ChainOnlyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_certs as certs;

    /// Fresh test certificates in a temporary directory, removed on drop.
    struct Fixtures(tempfile::TempDir);

    impl Fixtures {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let dir = tempfile::tempdir()?;
            certs::generate()?.write_to(dir.path())?;
            Ok(Self(dir))
        }

        fn path(&self, name: &str) -> String {
            self.0.path().join(name).display().to_string()
        }
    }

    const LONG: std::time::Duration = std::time::Duration::from_secs(60);

    fn params(mode: TlsMode) -> TlsParams {
        TlsParams {
            mode,
            ..TlsParams::default()
        }
    }

    #[tokio::test]
    async fn disable_needs_no_configuration() -> Result<(), DriverError> {
        let setup = TlsSetup::new(&params(TlsMode::Disable), "db.example.com", LONG).await?;
        assert!(setup.connector.is_none());
        assert!(matches!(setup.ssl_mode, SslMode::Disable));
        Ok(())
    }

    #[tokio::test]
    async fn modes_map_to_tokio_postgres() -> Result<(), Box<dyn std::error::Error>> {
        let fixtures = Fixtures::new()?;
        let prefer = TlsSetup::new(&params(TlsMode::Prefer), "localhost", LONG).await?;
        assert!(matches!(prefer.ssl_mode, SslMode::Prefer));
        let ca = TlsParams {
            ca_path: Some(fixtures.path("ca.crt")),
            ..params(TlsMode::VerifyCa)
        };
        assert!(matches!(
            TlsSetup::new(&ca, "localhost", LONG).await?.ssl_mode,
            SslMode::Require
        ));
        Ok(())
    }

    #[tokio::test]
    async fn server_name_overrides_host() -> Result<(), Box<dyn std::error::Error>> {
        let fixtures = Fixtures::new()?;
        let tls = TlsParams {
            ca_path: Some(fixtures.path("ca.crt")),
            server_name: Some("db.internal".into()),
            ..params(TlsMode::VerifyFull)
        };
        assert_eq!(
            TlsSetup::new(&tls, "127.0.0.1", LONG).await?.server_name,
            "db.internal"
        );
        assert_eq!(
            TlsSetup::new(&params(TlsMode::Require), "127.0.0.1", LONG)
                .await?
                .server_name,
            "127.0.0.1"
        );
        Ok(())
    }

    #[tokio::test]
    async fn verify_full_rejects_an_invalid_server_name() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixtures = Fixtures::new()?;
        let tls = TlsParams {
            ca_path: Some(fixtures.path("ca.crt")),
            server_name: Some("not a host".into()),
            ..params(TlsMode::VerifyFull)
        };
        let error = TlsSetup::new(&tls, "localhost", LONG).await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Tls));
        Ok(())
    }

    #[tokio::test]
    async fn loads_client_certificate_and_key() -> Result<(), Box<dyn std::error::Error>> {
        let fixtures = Fixtures::new()?;
        let tls = TlsParams {
            client_cert_path: Some(fixtures.path("client.crt")),
            client_key_path: Some(fixtures.path("client.key")),
            ..params(TlsMode::Require)
        };
        TlsSetup::new(&tls, "localhost", LONG).await?;
        Ok(())
    }

    #[tokio::test]
    async fn half_a_client_identity_is_an_error() -> Result<(), Box<dyn std::error::Error>> {
        let fixtures = Fixtures::new()?;
        let tls = TlsParams {
            client_cert_path: Some(fixtures.path("client.crt")),
            ..params(TlsMode::Require)
        };
        let error = TlsSetup::new(&tls, "localhost", LONG).await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Tls));
        Ok(())
    }

    #[tokio::test]
    async fn missing_ca_file_is_a_tls_error() {
        let tls = TlsParams {
            ca_path: Some("/nonexistent/varve/does-not-exist.crt".into()),
            ..params(TlsMode::VerifyCa)
        };
        let error = TlsSetup::new(&tls, "localhost", LONG).await.err();
        assert_eq!(error.as_ref().map(|e| e.kind), Some(ErrorKind::Tls));
        assert!(
            error
                .map(|e| e.message.contains("does-not-exist.crt"))
                .unwrap_or(false)
        );
    }
}
