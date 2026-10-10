//! The public side of the runner: the listeners, the inbound allowlist, TLS,
//! and what a public request may ask for.

use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Request};
use ipnet::IpNet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use uuid::Uuid;

use crate::logs::Stream;

/// A client must send all request headers in this time.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A client must end the TLS handshake in this time.
pub const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// An accept error is often a full file table, so the loop waits before
/// the next accept, or it takes a full core.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);

/// Headers that only the runner sets. A public request that sends them
/// loses them.
const RUNNER_HEADERS: [&str; 12] = [
    "x-worker-id",
    "x-worker-name",
    "x-openworkers-internal",
    "x-openworkers-depth",
    "x-request-id",
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-port",
    "x-forwarded-proto",
    "x-real-ip",
    "cf-connecting-ip",
];

/// What the HTTPS listener does with a client that sends no cert, when
/// HTTPS_CLIENT_CA_FILE is set. A cert that the CA did not sign always fails
/// the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCertMode {
    /// Close the connection.
    Require,
    /// Serve it and log a warning, to find the hosts without the cert before
    /// they are refused.
    Log,
}

#[derive(Debug)]
pub struct Config {
    pub http_addr: SocketAddr,
    pub https_addr: SocketAddr,
    /// Listeners per address, on one port with SO_REUSEPORT.
    pub listeners: usize,
    pub dashboard_hosts: Vec<String>,
    pub api_worker: String,
    pub worker_domains: Vec<String>,
    /// The peers that can connect to the public listeners; None lets all.
    pub allowlist: Option<Vec<IpNet>>,
    /// The header in which an allowlisted proxy gives the client address.
    pub client_ip_header: Option<HeaderName>,
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// Set with HTTPS_CLIENT_CA_FILE: the HTTPS clients present a cert that
    /// this CA signs.
    pub client_cert: Option<ClientCertMode>,
}

type ConfigError = Box<dyn std::error::Error + Send + Sync>;

fn list(value: Option<String>, default: &str) -> Vec<String> {
    value
        .as_deref()
        .unwrap_or(default)
        .split(',')
        .map(|item| item.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .collect()
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
    }

    /// Reads the configuration from `var`, which gives the value of a
    /// variable that is set and not empty.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let addr = |name: &str, default: &str| -> Result<SocketAddr, ConfigError> {
            let value = var(name).unwrap_or_else(|| default.to_string());

            value
                .parse()
                .map_err(|_| format!("{name} is not an address: {value}").into())
        };

        let listeners = match var("HTTP_LISTENERS") {
            Some(value) => match value.parse::<usize>() {
                Ok(count) if count > 0 => count,
                _ => return Err(format!("HTTP_LISTENERS is not a positive count: {value}").into()),
            },
            None => std::thread::available_parallelism().map_or(4, |count| count.get()),
        };

        let allowlist = match var("INBOUND_ALLOWLIST_FILE") {
            Some(path) => {
                let text = std::fs::read_to_string(&path)
                    .map_err(|error| format!("INBOUND_ALLOWLIST_FILE {path}: {error}"))?;
                Some(parse_allowlist(&text)?)
            }
            None => None,
        };

        let client_ip_header = match var("CLIENT_IP_HEADER") {
            Some(name) => {
                if allowlist.is_none() {
                    // Without an allowlist each client could give its own address
                    return Err("CLIENT_IP_HEADER needs INBOUND_ALLOWLIST_FILE".into());
                }

                Some(
                    HeaderName::try_from(name.as_str())
                        .map_err(|_| format!("CLIENT_IP_HEADER is not a header name: {name}"))?,
                )
            }
            None => None,
        };

        let client_ca = var("HTTPS_CLIENT_CA_FILE");

        let client_cert = match (&client_ca, var("HTTPS_CLIENT_CERT_MODE").as_deref()) {
            (None, None) => None,
            (None, Some(_)) => {
                return Err("HTTPS_CLIENT_CERT_MODE needs HTTPS_CLIENT_CA_FILE".into());
            }
            (Some(_), None | Some("require")) => Some(ClientCertMode::Require),
            (Some(_), Some("log")) => Some(ClientCertMode::Log),
            (Some(_), Some(mode)) => {
                return Err(format!("HTTPS_CLIENT_CERT_MODE is require or log, not {mode}").into());
            }
        };

        let tls = match (var("HTTP_TLS_CERTIFICATE"), var("HTTP_TLS_KEY")) {
            (None, None) if client_ca.is_some() => {
                return Err(
                    "HTTPS_CLIENT_CA_FILE needs HTTP_TLS_CERTIFICATE and HTTP_TLS_KEY".into(),
                );
            }
            (None, None) => None,
            (Some(certificate), Some(key)) => {
                Some(tls_config(&certificate, &key, client_ca.as_deref())?)
            }
            _ => return Err("set both HTTP_TLS_CERTIFICATE and HTTP_TLS_KEY, or neither".into()),
        };

        Ok(Self {
            http_addr: addr("HTTP_ADDR", "0.0.0.0:8081")?,
            https_addr: addr("HTTPS_ADDR", "0.0.0.0:8443")?,
            listeners,
            dashboard_hosts: list(
                var("DASHBOARD_HOSTS"),
                "dash.openworkers.com,dash.openworkers.dev,dash.dev.localhost,dash.dev.kube",
            ),
            api_worker: var("API_WORKER_NAME").unwrap_or_else(|| "openworkers-api".to_string()),
            worker_domains: list(var("WORKER_DOMAINS"), ""),
            allowlist,
            client_ip_header,
            tls,
            client_cert,
        })
    }

    pub fn allows(&self, peer: IpAddr) -> bool {
        allowed(self.allowlist.as_deref(), peer)
    }
}

/// One IPv4 or IPv6 address or network per line; `#` starts a comment.
pub fn parse_allowlist(text: &str) -> Result<Vec<IpNet>, String> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let value = line.split('#').next().unwrap_or("").trim();

            if value.is_empty() {
                return None;
            }

            let network = value
                .parse::<IpNet>()
                .or_else(|_| value.parse::<IpAddr>().map(IpNet::from))
                .map_err(|_| format!("allowlist line {}: not an address: {value}", index + 1));

            Some(network)
        })
        .collect()
}

pub fn allowed(allowlist: Option<&[IpNet]>, peer: IpAddr) -> bool {
    let peer = peer.to_canonical();

    allowlist.is_none_or(|networks| networks.iter().any(|network| network.contains(&peer)))
}

fn tls_config(
    certificate: &str,
    key: &str,
    client_ca: Option<&str>,
) -> Result<Arc<rustls::ServerConfig>, ConfigError> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let chain = CertificateDer::pem_file_iter(certificate)
        .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
        .map_err(|error| format!("HTTP_TLS_CERTIFICATE {certificate}: {error}"))?;

    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|error| format!("HTTP_TLS_KEY {key}: {error}"))?;

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?;

    let builder = match client_ca {
        None => builder.with_no_client_auth(),
        Some(path) => {
            let mut roots = rustls::RootCertStore::empty();
            let certificates = CertificateDer::pem_file_iter(path)
                .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
                .map_err(|error| format!("HTTPS_CLIENT_CA_FILE {path}: {error}"))?;

            for certificate in certificates {
                roots.add(certificate)?;
            }

            if roots.is_empty() {
                return Err(format!("HTTPS_CLIENT_CA_FILE {path}: no certificate").into());
            }

            // A client without a cert ends the handshake; client_cert_allowed decides
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                provider,
            )
            .allow_unauthenticated()
            .build()?;

            builder.with_client_cert_verifier(verifier)
        }
    };

    let mut config = builder.with_single_cert(chain, key)?;

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

/// Whether the HTTPS listener serves a connection after its handshake: rustls
/// already refused a cert that the CA did not sign, so this decides on a
/// connection without a cert.
pub fn client_cert_allowed(
    mode: Option<ClientCertMode>,
    connection: &rustls::ServerConnection,
    peer: SocketAddr,
) -> bool {
    let Some(mode) = mode else {
        return true;
    };

    if connection.peer_certificates().is_some() {
        return true;
    }

    let host = connection.server_name().unwrap_or("-");

    match mode {
        ClientCertMode::Require => {
            tracing::warn!(%peer, %host, "no client cert; connection refused");
            false
        }
        ClientCertMode::Log => {
            tracing::warn!(%peer, %host, "no client cert; served, as HTTPS_CLIENT_CERT_MODE=log");
            true
        }
    }
}

/// Binds `count` listeners to one address with SO_REUSEPORT, so the kernel
/// spreads the connections over their accept loops.
pub fn bind(addr: SocketAddr, count: usize) -> std::io::Result<Vec<TcpListener>> {
    let mut addr = addr;
    let mut listeners = Vec::with_capacity(count);

    for _ in 0..count {
        let socket = match addr {
            SocketAddr::V4(_) => TcpSocket::new_v4()?,
            SocketAddr::V6(_) => TcpSocket::new_v6()?,
        };

        socket.set_reuseaddr(true)?;
        socket.set_reuseport(true)?;
        socket.bind(addr)?;

        let listener = socket.listen(1024)?;

        // Port 0 takes a free port once; the other listeners join it
        addr = listener.local_addr()?;
        listeners.push(listener);
    }

    Ok(listeners)
}

/// Accepts connections without end. A peer that the allowlist refuses is
/// closed before any byte is read.
pub async fn accept(
    listener: TcpListener,
    allowlist: Option<Arc<[IpNet]>>,
    mut serve: impl FnMut(TcpStream, SocketAddr),
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                if allowed(allowlist.as_deref(), peer.ip()) {
                    serve(stream, peer);
                } else {
                    tracing::debug!(%peer, "connection refused by the inbound allowlist");
                }
            }
            Err(error) => {
                tracing::error!(%error, "accept failed");
                tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
            }
        }
    }
}

/// What a public request asks for, after `prepare`.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// A worker: the runner resolves it from the headers and the host.
    Worker,
    Logs(Uuid, Stream),
    /// The latency probe of the dashboard: the runner answers it.
    Latency,
    /// A path that only a scanner asks for; it gets a 404 and no worker runs.
    Probe,
    BadHost,
}

/// Replaces the headers that only the runner sets, and gives the route of a
/// public request. `tls` tells how the client connected to the runner.
pub fn prepare<B>(config: &Config, req: &mut Request<B>, peer: SocketAddr, tls: bool) -> Route {
    // HTTP/2 and absolute-form requests name the host in the target, which
    // takes precedence over a Host header
    if let Some(authority) = req.uri().authority().cloned() {
        if let Ok(value) = HeaderValue::from_str(authority.as_str()) {
            req.headers_mut().insert(hyper::header::HOST, value);
        }

        let path = req.uri().path_and_query().map_or("/", |path| path.as_str());
        *req.uri_mut() = path.parse().expect("a path of a valid URI is a valid URI");
    }

    let forwarded = forwarded_by_proxy(config, req.headers(), peer.ip());
    let headers = req.headers_mut();

    for name in RUNNER_HEADERS {
        headers.remove(name);
    }

    let client = forwarded.client.unwrap_or(peer.ip()).to_canonical();
    let client = HeaderValue::from_str(&client.to_string()).expect("an address is a header value");
    let scheme = match forwarded.https.unwrap_or(tls) {
        true => "https",
        false => "http",
    };

    headers.insert("x-request-id", request_id());
    headers.insert("x-forwarded-proto", HeaderValue::from_static(scheme));
    headers.insert("x-real-ip", client.clone());
    headers.insert("x-forwarded-for", client.clone());
    // Workers written for Cloudflare read the client address here
    headers.insert("cf-connecting-ip", client);

    let Some(host) = host(headers) else {
        return Route::BadHost;
    };

    headers.insert(
        "x-forwarded-host",
        HeaderValue::from_str(&host).expect("a parsed host is a header value"),
    );

    if scanner_probe(req.uri().path()) {
        return Route::Probe;
    }

    if config.dashboard_hosts.contains(&host) {
        if let Some((worker_id, stream)) = crate::logs::route(req.uri().path()) {
            return Route::Logs(worker_id, stream);
        }

        if req.uri().path() == "/api/health/latency/proxy" {
            return Route::Latency;
        }

        if worker_upload(req.uri().path()) {
            let limit = crate::request_body::BodyLimit(crate::request_body::UPLOAD_MAX_BODY_BYTES);
            req.extensions_mut().insert(limit);
        }

        let api_worker =
            HeaderValue::from_str(&config.api_worker).expect("API_WORKER_NAME is a header value");
        req.headers_mut().insert("x-worker-name", api_worker);

        return Route::Worker;
    }

    if let Some((header, name)) = worker_route(&host, &config.worker_domains) {
        let name = HeaderValue::from_str(name).expect("a host label is a header value");
        req.headers_mut().insert(header, name);
    }

    Route::Worker
}

/// Whether a path is the worker upload of the dashboard API:
/// `/api/v1/workers/{id}/upload`.
fn worker_upload(path: &str) -> bool {
    path.strip_prefix("/api/v1/workers/")
        .and_then(|rest| rest.strip_suffix("/upload"))
        .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

fn request_id() -> HeaderValue {
    HeaderValue::from_str(&Uuid::new_v4().to_string()).expect("a UUID is a header value")
}

/// What an allowlisted proxy says about the client.
#[derive(Debug, Default, PartialEq, Eq)]
struct Forwarded {
    client: Option<IpAddr>,
    https: Option<bool>,
}

fn forwarded_by_proxy(config: &Config, headers: &HeaderMap, peer: IpAddr) -> Forwarded {
    let Some(header) = &config.client_ip_header else {
        return Forwarded::default();
    };

    if !config.allows(peer) {
        return Forwarded::default();
    }

    let client = headers
        .get(header)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok());

    let https = match headers.get("x-forwarded-proto").map(HeaderValue::as_bytes) {
        Some(b"https") => Some(true),
        Some(b"http") => Some(false),
        _ => None,
    };

    Forwarded { client, https }
}

/// The host of the Host header, without port and final dot, in lowercase.
pub fn host(headers: &HeaderMap) -> Option<String> {
    let authority: hyper::http::uri::Authority = headers
        .get(hyper::header::HOST)?
        .to_str()
        .ok()?
        .parse()
        .ok()?;

    let host = authority.host().trim_end_matches('.').to_ascii_lowercase();

    (!host.is_empty()).then_some(host)
}

/// The worker a host names under a worker domain: `{name}.{domain}` gives
/// x-worker-name, and `{uuid}.{domain}` gives x-worker-id.
pub fn worker_route<'a>(host: &'a str, domains: &[String]) -> Option<(&'static str, &'a str)> {
    let name = domains
        .iter()
        .find_map(|domain| host.strip_suffix(domain.as_str())?.strip_suffix('.'))?;

    if name.is_empty() || name.contains('.') {
        return None;
    }

    match Uuid::parse_str(name) {
        Ok(_) => Some(("x-worker-id", name)),
        Err(_) => Some(("x-worker-name", name)),
    }
}

/// Dot files that only a scanner asks for, in lowercase.
const PROBED_DOT_FILES: [&str; 9] = [
    "env", "git", "svn", "hg", "ssh", "aws", "htpasswd", "htaccess", "ds_store",
];

const PROBED_KEYS: [&str; 4] = ["rsa", "dsa", "ecdsa", "ed25519"];

/// Whether a path is one that only a scanner asks for: a dot file of a
/// tool, a private key, /etc/passwd, or a PHP script.
pub fn scanner_probe(path: &str) -> bool {
    let path = percent_encoding::percent_decode_str(path)
        .decode_utf8_lossy()
        .to_ascii_lowercase();

    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    if matches!(segments.as_slice(), ["etc", "passwd" | "shadow"]) {
        return true;
    }

    let key = segments.last().is_some_and(|name| {
        name.strip_prefix("id_")
            .map(|name| name.strip_suffix(".pub").unwrap_or(name))
            .is_some_and(|kind| PROBED_KEYS.contains(&kind))
    });

    key || segments.iter().any(|segment| {
        let dot_file = segment
            .strip_prefix('.')
            .and_then(|name| name.split('.').next())
            .is_some_and(|stem| PROBED_DOT_FILES.contains(&stem));

        let php = segment.rsplit_once('.').is_some_and(|(_, extension)| {
            extension == "phtml"
                || extension.strip_prefix("php").is_some_and(|version| {
                    version.len() <= 1 && version.bytes().all(|b| b.is_ascii_digit())
                })
        });

        dot_file || php
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();

        Config::from_vars(|name| vars.get(name).cloned())
    }

    fn allowlist_file(text: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), text).unwrap();
        file
    }

    fn request(pairs: &[(&str, &str)], uri: &str) -> Request<()> {
        let mut builder = Request::builder().uri(uri);

        for (name, value) in pairs {
            builder = builder.header(*name, *value);
        }

        builder.body(()).unwrap()
    }

    const PEER: &str = "192.0.2.1:4000";

    #[test]
    fn the_default_configuration_has_no_allowlist_tls_or_worker_domain() {
        let config = config(&[]).unwrap();

        assert_eq!(config.http_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.https_addr, "0.0.0.0:8443".parse().unwrap());
        assert!(config.listeners > 0);
        assert!(config.allowlist.is_none());
        assert!(config.client_ip_header.is_none());
        assert!(config.tls.is_none());
        assert!(config.worker_domains.is_empty());
        assert_eq!(config.api_worker, "openworkers-api");
        assert!(
            config
                .dashboard_hosts
                .contains(&"dash.openworkers.com".to_string())
        );
    }

    #[test]
    fn a_configuration_that_is_not_valid_stops_the_start() {
        let list = allowlist_file("192.0.2.0/24\n");

        for vars in [
            vec![("HTTP_ADDR", "8081")],
            vec![("HTTP_LISTENERS", "0")],
            vec![("INBOUND_ALLOWLIST_FILE", "/nonexistent/allowlist")],
            vec![("CLIENT_IP_HEADER", "cf-connecting-ip")],
            vec![
                ("INBOUND_ALLOWLIST_FILE", list.path().to_str().unwrap()),
                ("CLIENT_IP_HEADER", "bad header"),
            ],
            vec![("HTTP_TLS_CERTIFICATE", "/nonexistent/cert.pem")],
            vec![
                ("HTTP_TLS_CERTIFICATE", "/nonexistent/cert.pem"),
                ("HTTP_TLS_KEY", "/nonexistent/key.pem"),
            ],
        ] {
            assert!(config(&vars).is_err(), "{vars:?}");
        }

        let config = config(&[
            ("INBOUND_ALLOWLIST_FILE", list.path().to_str().unwrap()),
            ("CLIENT_IP_HEADER", "CF-Connecting-IP"),
            ("WORKER_DOMAINS", " Workers.Rocks. , ,workers.dev.localhost"),
        ])
        .unwrap();

        assert_eq!(config.client_ip_header.unwrap(), "cf-connecting-ip");
        assert_eq!(
            config.worker_domains,
            ["workers.rocks", "workers.dev.localhost"]
        );
    }

    #[test]
    fn the_client_ca_needs_tls_and_a_known_mode() {
        let tls = [
            (
                "HTTP_TLS_CERTIFICATE",
                "tests/fixtures/client_cert/server.pem",
            ),
            ("HTTP_TLS_KEY", "tests/fixtures/client_cert/server.key"),
        ];
        let ca = ("HTTPS_CLIENT_CA_FILE", "tests/fixtures/client_cert/ca.pem");
        let with = |extra: &[(&'static str, &'static str)]| {
            let mut vars = tls.to_vec();
            vars.extend_from_slice(extra);
            config(&vars)
        };

        assert_eq!(with(&[]).unwrap().client_cert, None);
        assert_eq!(
            with(&[ca]).unwrap().client_cert,
            Some(ClientCertMode::Require)
        );
        assert_eq!(
            with(&[ca, ("HTTPS_CLIENT_CERT_MODE", "log")])
                .unwrap()
                .client_cert,
            Some(ClientCertMode::Log)
        );

        assert!(with(&[ca, ("HTTPS_CLIENT_CERT_MODE", "maybe")]).is_err());
        assert!(with(&[("HTTPS_CLIENT_CERT_MODE", "log")]).is_err());
        assert!(with(&[("HTTPS_CLIENT_CA_FILE", "/nonexistent/ca.pem")]).is_err());
        assert!(
            with(&[(
                "HTTPS_CLIENT_CA_FILE",
                "tests/fixtures/client_cert/server.key"
            )])
            .is_err()
        );
        assert!(config(&[ca]).is_err(), "a client CA without TLS");
    }

    #[test]
    fn the_allowlist_takes_addresses_networks_and_comments() {
        let networks =
            parse_allowlist("# proxies\n192.0.2.0/24\n2001:db8::/32 # v6\n\n127.0.0.1\n").unwrap();

        for ip in ["192.0.2.8", "2001:db8::1", "127.0.0.1", "::ffff:192.0.2.8"] {
            assert!(allowed(Some(&networks), ip.parse().unwrap()), "{ip}");
        }

        assert!(!allowed(Some(&networks), "198.51.100.1".parse().unwrap()));
        assert!(!allowed(Some(&[]), "127.0.0.1".parse().unwrap()));
        assert!(allowed(None, "198.51.100.1".parse().unwrap()));

        assert_eq!(
            parse_allowlist("192.0.2.0/24\n192.0.2.0/999\n"),
            Err("allowlist line 2: not an address: 192.0.2.0/999".to_string())
        );
    }

    #[test]
    fn a_public_request_cannot_set_the_headers_of_the_runner() {
        let config = config(&[("WORKER_DOMAINS", "workers.rocks")]).unwrap();
        let mut forged: Vec<(&str, &str)> = RUNNER_HEADERS
            .iter()
            .map(|name| (*name, "203.0.113.9"))
            .collect();
        forged.push(("host", "hello.workers.rocks:443"));

        let mut req = request(&forged, "/path?q=1");

        assert_eq!(
            prepare(&config, &mut req, PEER.parse().unwrap(), true),
            Route::Worker
        );

        let headers = req.headers();
        assert_eq!(headers["x-worker-name"], "hello");
        assert!(!headers.contains_key("x-worker-id"));
        assert!(!headers.contains_key("x-openworkers-internal"));
        assert!(!headers.contains_key("x-openworkers-depth"));
        assert!(!headers.contains_key("forwarded"));
        assert!(!headers.contains_key("x-forwarded-port"));
        assert_ne!(headers["x-request-id"], "203.0.113.9");
        assert_eq!(headers["x-forwarded-proto"], "https");
        assert_eq!(headers["x-forwarded-host"], "hello.workers.rocks");

        for name in ["x-real-ip", "x-forwarded-for", "cf-connecting-ip"] {
            assert_eq!(headers[name], "192.0.2.1", "{name}");
        }
    }

    #[test]
    fn an_allowlisted_proxy_gives_the_client_address_and_scheme() {
        let list = allowlist_file("192.0.2.0/24\n");
        let config = config(&[
            ("INBOUND_ALLOWLIST_FILE", list.path().to_str().unwrap()),
            ("CLIENT_IP_HEADER", "cf-connecting-ip"),
        ])
        .unwrap();

        let forwarded = [
            ("host", "example.com"),
            ("cf-connecting-ip", " 2001:db8::7 "),
            ("x-forwarded-proto", "https"),
        ];

        let mut req = request(&forwarded, "/");
        prepare(&config, &mut req, PEER.parse().unwrap(), false);
        assert_eq!(req.headers()["x-real-ip"], "2001:db8::7");
        assert_eq!(req.headers()["cf-connecting-ip"], "2001:db8::7");
        assert_eq!(req.headers()["x-forwarded-proto"], "https");

        // A peer outside the allowlist does not reach prepare in production;
        // if it does, its headers are not trusted
        let mut req = request(&forwarded, "/");
        prepare(
            &config,
            &mut req,
            "198.51.100.1:4000".parse().unwrap(),
            false,
        );
        assert_eq!(req.headers()["x-real-ip"], "198.51.100.1");
        assert_eq!(req.headers()["x-forwarded-proto"], "http");

        let mut req = request(
            &[("host", "example.com"), ("cf-connecting-ip", "not an ip")],
            "/",
        );
        prepare(&config, &mut req, PEER.parse().unwrap(), true);
        assert_eq!(req.headers()["x-real-ip"], "192.0.2.1");
    }

    #[test]
    fn without_a_client_ip_header_the_proxy_headers_are_not_trusted() {
        let list = allowlist_file("192.0.2.0/24\n");
        let config = config(&[("INBOUND_ALLOWLIST_FILE", list.path().to_str().unwrap())]).unwrap();

        let mut req = request(
            &[
                ("host", "example.com"),
                ("cf-connecting-ip", "203.0.113.9"),
                ("x-forwarded-proto", "https"),
            ],
            "/",
        );
        prepare(&config, &mut req, PEER.parse().unwrap(), false);

        assert_eq!(req.headers()["x-real-ip"], "192.0.2.1");
        assert_eq!(req.headers()["x-forwarded-proto"], "http");
    }

    #[test]
    fn dashboard_hosts_go_to_the_api_worker_except_logs_and_latency() {
        let config = config(&[]).unwrap();
        let id = Uuid::new_v4();
        let route = |path: &str| {
            let mut req = request(&[("host", "Dash.OpenWorkers.com")], path);
            let route = prepare(&config, &mut req, PEER.parse().unwrap(), true);
            (route, req.headers().get("x-worker-name").cloned())
        };

        assert_eq!(
            route("/workers"),
            (
                Route::Worker,
                Some(HeaderValue::from_static("openworkers-api"))
            )
        );
        assert_eq!(
            route(&format!("/api/v1/workers/{id}/logs")),
            (Route::Logs(id, Stream::Sse), None)
        );
        assert_eq!(
            route(&format!("/api/v1/workers/{id}/ws-logs")),
            (Route::Logs(id, Stream::WebSocket), None)
        );
        assert_eq!(route("/api/health/latency/proxy"), (Route::Latency, None));
    }

    #[test]
    fn only_a_dashboard_upload_gets_the_upload_body_limit() {
        use crate::request_body::{BodyLimit, UPLOAD_MAX_BODY_BYTES};

        let config = config(&[("WORKER_DOMAINS", "workers.rocks")]).unwrap();
        let limit = |host: &str, path: &str| {
            let mut req = request(&[("host", host)], path);
            prepare(&config, &mut req, PEER.parse().unwrap(), true);
            req.extensions().get::<BodyLimit>().copied()
        };

        let upload = "/api/v1/workers/my-worker/upload";

        assert_eq!(
            limit("dash.openworkers.com", upload),
            Some(BodyLimit(UPLOAD_MAX_BODY_BYTES))
        );
        assert_eq!(limit("hello.workers.rocks", upload), None);
        assert_eq!(limit("www.example.com", upload), None);

        for path in [
            "/api/v1/workers/my-worker",
            "/api/v1/workers//upload",
            "/api/v1/workers/a/b/upload",
            "/api/v1/workers/my-worker/upload/more",
        ] {
            assert_eq!(limit("dash.openworkers.com", path), None, "{path}");
        }
    }

    #[test]
    fn other_hosts_keep_their_host_for_the_database_routes() {
        let config = config(&[("WORKER_DOMAINS", "workers.rocks")]).unwrap();
        let mut req = request(&[("host", "www.example.com")], "/");

        assert_eq!(
            prepare(&config, &mut req, PEER.parse().unwrap(), true),
            Route::Worker
        );
        assert!(!req.headers().contains_key("x-worker-name"));
        assert!(!req.headers().contains_key("x-worker-id"));
        assert_eq!(host(req.headers()).as_deref(), Some("www.example.com"));
    }

    #[test]
    fn the_target_authority_takes_precedence_over_the_host_header() {
        let config = config(&[]).unwrap();
        let mut req = request(
            &[("host", "dash.openworkers.com")],
            "https://hello.example:8443/path?q=1",
        );

        prepare(&config, &mut req, PEER.parse().unwrap(), true);

        assert_eq!(host(req.headers()).as_deref(), Some("hello.example"));
        assert_eq!(req.uri().to_string(), "/path?q=1");
        assert!(!req.headers().contains_key("x-worker-name"));
    }

    #[test]
    fn a_request_without_a_host_is_refused() {
        let config = config(&[]).unwrap();

        for headers in [vec![], vec![("host", "")], vec![("host", "bad host")]] {
            let mut req = request(&headers, "/");
            assert_eq!(
                prepare(&config, &mut req, PEER.parse().unwrap(), false),
                Route::BadHost,
                "{headers:?}"
            );
        }
    }

    #[test]
    fn worker_domains_need_one_label_before_a_complete_suffix() {
        let domains = vec!["workers.rocks".to_string()];
        let id = Uuid::new_v4().to_string();
        let host = format!("{id}.workers.rocks");

        assert_eq!(
            worker_route("hello.workers.rocks", &domains),
            Some(("x-worker-name", "hello"))
        );
        assert_eq!(
            worker_route(&host, &domains),
            Some(("x-worker-id", id.as_str()))
        );

        for host in [
            "workers.rocks",
            ".workers.rocks",
            "hello.notworkers.rocks",
            "nested.hello.workers.rocks",
            "hello.workers.rocks.evil.test",
        ] {
            assert_eq!(worker_route(host, &domains), None, "{host}");
        }
    }

    #[test]
    fn scanner_paths_are_probes_and_others_are_not() {
        for path in [
            "/.env",
            "/.env.local",
            "/app/.git/config",
            "/%2eenv",
            "/.DS_Store",
            "/.aws/credentials",
            "/home/user/.ssh/id_rsa",
            "/id_ed25519.pub",
            "/etc/passwd",
            "//etc//shadow",
            "/wp-login.php",
            "/index.PHP5",
            "/cgi/test.phtml",
            "/x.php/extra",
        ] {
            assert!(scanner_probe(path), "{path}");
        }

        for path in [
            "/",
            "/.well-known/acme-challenge/token",
            "/.environment",
            "/environment",
            "/assets/app.js",
            "/php/info",
            "/file.php.txt",
            "/id_rsa_backup",
            "/docs/etc/passwd",
            "/x.php55",
        ] {
            assert!(!scanner_probe(path), "{path}");
        }
    }

    #[tokio::test]
    async fn listeners_share_one_port() {
        let listeners = bind("127.0.0.1:0".parse().unwrap(), 3).unwrap();
        let port = listeners[0].local_addr().unwrap().port();

        assert_eq!(listeners.len(), 3);
        assert!(
            listeners
                .iter()
                .all(|listener| listener.local_addr().unwrap().port() == port)
        );
    }

    #[tokio::test]
    async fn a_peer_outside_the_allowlist_is_closed_before_it_is_served() {
        use tokio::io::AsyncReadExt;

        let listener = bind("127.0.0.1:0".parse().unwrap(), 1).unwrap().remove(0);
        let addr = listener.local_addr().unwrap();
        let allowlist: Arc<[IpNet]> = Arc::from(parse_allowlist("192.0.2.0/24").unwrap());
        let (served, mut served_rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn(accept(listener, Some(allowlist), move |_, peer| {
            served.send(peer).unwrap();
        }));

        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut buffer = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buffer))
            .await
            .unwrap();

        assert!(
            matches!(read, Ok(0) | Err(_)),
            "the connection stays open: {read:?}"
        );
        assert!(served_rx.try_recv().is_err());
    }
}
