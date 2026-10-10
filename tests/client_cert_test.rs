//! The HTTPS listener with HTTPS_CLIENT_CA_FILE: a real handshake for a client
//! with a cert of the CA, without a cert, and with a cert of another CA.

use std::collections::HashMap;
use std::sync::Arc;

use openworkers_runner::ingress::{ClientCertMode, Config, client_cert_allowed};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::net::{TcpListener, TcpStream};

const FIXTURES: &str = "tests/fixtures/client_cert";

/// The test reads what the server decides; it does not check the server cert.
#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn config(mode: &str) -> Config {
    let vars: HashMap<&str, String> = [
        ("HTTP_TLS_CERTIFICATE", format!("{FIXTURES}/server.pem")),
        ("HTTP_TLS_KEY", format!("{FIXTURES}/server.key")),
        ("HTTPS_CLIENT_CA_FILE", format!("{FIXTURES}/ca.pem")),
        ("HTTPS_CLIENT_CERT_MODE", mode.to_string()),
    ]
    .into();

    Config::from_vars(|name| vars.get(name).cloned()).unwrap()
}

/// Connects with the client cert `name` (or none) and gives what the server
/// decides: None for a failed handshake, else client_cert_allowed.
async fn server_decision(config: &Config, name: Option<&str>) -> Option<bool> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(config.tls.clone().unwrap());
    let mode = config.client_cert;

    let server = tokio::spawn(async move {
        let (tcp, peer) = listener.accept().await.unwrap();
        let stream = acceptor.accept(tcp).await.ok()?;

        Some(client_cert_allowed(mode, stream.get_ref().1, peer))
    });

    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify(provider)));

    let client = match name {
        Some(name) => {
            let chain = CertificateDer::pem_file_iter(format!("{FIXTURES}/{name}.pem"))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let key = PrivateKeyDer::from_pem_file(format!("{FIXTURES}/{name}.key")).unwrap();
            builder.with_client_auth_cert(chain, key).unwrap()
        }
        None => builder.with_no_client_auth(),
    };

    let tcp = TcpStream::connect(addr).await.unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let _client = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await;

    server.await.unwrap()
}

#[tokio::test]
async fn require_serves_a_cert_of_the_ca_only() {
    let config = config("require");

    assert_eq!(config.client_cert, Some(ClientCertMode::Require));
    assert_eq!(server_decision(&config, Some("proxy")).await, Some(true));
    assert_eq!(server_decision(&config, None).await, Some(false));
    assert_eq!(
        server_decision(&config, Some("stranger")).await,
        None,
        "a cert of another CA fails the handshake"
    );
}

#[tokio::test]
async fn log_serves_a_client_without_a_cert_but_not_a_cert_of_another_ca() {
    let config = config("log");

    assert_eq!(server_decision(&config, Some("proxy")).await, Some(true));
    assert_eq!(server_decision(&config, None).await, Some(true));
    assert_eq!(server_decision(&config, Some("stranger")).await, None);
}
