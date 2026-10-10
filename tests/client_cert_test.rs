//! The HTTPS listener with HTTPS_CLIENT_CA_FILE: a real handshake for a client
//! with a cert of the CA, without a cert, and with a cert of another CA, over
//! TCP and over HTTPS_SOCKET.

use std::collections::HashMap;
use std::sync::Arc;

use openworkers_runner::ingress::{
    ClientCertMode, Config, HttpsAddr, Peer, accept_unix, bind_unix, client_cert_allowed,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::net::{TcpListener, TcpStream, UnixStream};

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

fn config(vars: &[(&'static str, String)]) -> Config {
    let mut all: HashMap<&str, String> = [
        ("HTTP_TLS_CERTIFICATE", format!("{FIXTURES}/server.pem")),
        ("HTTP_TLS_KEY", format!("{FIXTURES}/server.key")),
        ("HTTPS_CLIENT_CA_FILE", format!("{FIXTURES}/ca.pem")),
    ]
    .into();
    all.extend(vars.iter().cloned());

    Config::from_vars(|name| all.get(name).cloned()).unwrap()
}

fn with_mode(mode: &str) -> Config {
    config(&[("HTTPS_CLIENT_CERT_MODE", mode.to_string())])
}

/// A client that offers h2 and http/1.1, with the client cert `name` (or none).
fn client(name: Option<&str>) -> tokio_rustls::TlsConnector {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify(provider)));

    let mut client = match name {
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

    client.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    tokio_rustls::TlsConnector::from(Arc::new(client))
}

fn localhost() -> ServerName<'static> {
    ServerName::try_from("localhost").unwrap()
}

/// Connects with the client cert `name` (or none) and gives what the server
/// decides: None for a failed handshake, else client_cert_allowed.
async fn server_decision(config: &Config, name: Option<&str>) -> Option<bool> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(config.tls.clone().unwrap());
    let mode = config.client_cert;

    let server = tokio::spawn(async move {
        let (tcp, peer) = listener.accept().await.unwrap();
        let stream = acceptor.accept(tcp).await.ok()?;

        Some(client_cert_allowed(
            mode,
            stream.get_ref().1,
            Peer::Tcp(peer),
        ))
    });

    let tcp = TcpStream::connect(addr).await.unwrap();
    let _client = client(name).connect(localhost(), tcp).await;

    server.await.unwrap()
}

#[tokio::test]
async fn require_serves_a_cert_of_the_ca_only() {
    let config = with_mode("require");

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
    let config = with_mode("log");

    assert_eq!(server_decision(&config, Some("proxy")).await, Some(true));
    assert_eq!(server_decision(&config, None).await, Some(true));
    assert_eq!(server_decision(&config, Some("stranger")).await, None);
}

#[tokio::test]
async fn https_socket_negotiates_h2_and_checks_the_client_cert() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("https.sock");
    let config = config(&[("HTTPS_SOCKET", path.to_str().unwrap().to_string())]);

    assert_eq!(config.https_addr, HttpsAddr::Unix(path.clone()));

    let acceptor = tokio_rustls::TlsAcceptor::from(config.tls.clone().unwrap());
    let mode = config.client_cert;
    let (decided, mut decisions) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(accept_unix(bind_unix(&path).unwrap(), move |stream| {
        let handshake = acceptor.accept(stream);
        let decided = decided.clone();

        tokio::spawn(async move {
            let decision = handshake
                .await
                .ok()
                .map(|stream| client_cert_allowed(mode, stream.get_ref().1, Peer::Unix));
            decided.send(decision).unwrap();
        });
    }));

    let stream = UnixStream::connect(&path).await.unwrap();
    let proxy = client(Some("proxy"))
        .connect(localhost(), stream)
        .await
        .unwrap();
    assert_eq!(proxy.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    assert_eq!(decisions.recv().await.unwrap(), Some(true));

    let stream = UnixStream::connect(&path).await.unwrap();
    let _anonymous = client(None).connect(localhost(), stream).await;
    assert_eq!(decisions.recv().await.unwrap(), Some(false));
}
