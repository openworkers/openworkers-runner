use hyper::Request;
use std::net::SocketAddr;
use std::sync::Arc;

pub struct Config {
    pub dashboard_hosts: Vec<String>,
    pub api_worker: String,
    pub allowlist: Option<Vec<ipnet::IpNet>>,
}
fn hosts(name: &str, default: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}
impl Config {
    pub fn from_env() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let allowlist = std::env::var("INBOUND_ALLOWLIST_FILE")
            .ok()
            .filter(|path| !path.is_empty())
            .map(
                |path| -> Result<_, Box<dyn std::error::Error + Send + Sync>> {
                    Ok(parse_allowlist(&std::fs::read_to_string(path)?)?)
                },
            )
            .transpose()?;
        Ok(Self {
            dashboard_hosts: hosts(
                "DASHBOARD_HOSTS",
                "dash.openworkers.com,dash.openworkers.dev,dash.dev.localhost,dash.dev.kube",
            ),
            api_worker: std::env::var("API_WORKER_NAME")
                .unwrap_or_else(|_| "openworkers-api".into()),
            allowlist,
        })
    }
}

pub fn parse_allowlist(text: &str) -> Result<Vec<ipnet::IpNet>, String> {
    text.lines()
        .enumerate()
        .filter_map(|(line, value)| {
            let value = value.split('#').next().unwrap_or("").trim();
            if value.is_empty() {
                return None;
            }
            Some(
                value
                    .parse::<ipnet::IpNet>()
                    .or_else(|_| value.parse::<std::net::IpAddr>().map(ipnet::IpNet::from))
                    .map_err(|_| format!("invalid inbound address on line {}", line + 1)),
            )
        })
        .collect()
}

pub fn allowed(allowlist: Option<&[ipnet::IpNet]>, peer: std::net::IpAddr) -> bool {
    allowlist.is_none_or(|networks| {
        networks
            .iter()
            .any(|net| net.contains(&peer.to_canonical()))
    })
}

pub fn host(headers: &hyper::HeaderMap) -> Option<String> {
    headers
        .get("host")?
        .to_str()
        .ok()?
        .parse::<hyper::http::uri::Authority>()
        .ok()
        .map(|a| a.host().trim_end_matches('.').to_ascii_lowercase())
}

pub fn normalize<B>(req: &mut Request<B>, peer: SocketAddr, tls: bool) {
    let authority = req.uri().authority().map(|a| a.as_str().to_owned());
    if req.uri().authority().is_some() {
        *req.uri_mut() = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .parse()
            .unwrap();
    }
    let headers = req.headers_mut();
    if !headers.contains_key("host")
        && let Some(authority) = authority
    {
        headers.insert("host", authority.parse().unwrap());
    }
    for name in [
        "x-worker-id",
        "x-worker-name",
        "x-openworkers-internal",
        "x-openworkers-depth",
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-port",
        "x-forwarded-proto",
        "x-real-ip",
        "cf-connecting-ip",
    ] {
        headers.remove(name);
    }
    headers.insert(
        "x-request-id",
        uuid::Uuid::new_v4().to_string().parse().unwrap(),
    );
    headers.insert(
        "x-forwarded-proto",
        if tls { "https" } else { "http" }.parse().unwrap(),
    );
    headers.insert("x-real-ip", peer.ip().to_string().parse().unwrap());
    headers.insert("x-forwarded-for", peer.ip().to_string().parse().unwrap());
}

pub fn worker_route<'a>(hostname: &'a str, domains: &[String]) -> Option<(&'static str, &'a str)> {
    let name = domains
        .iter()
        .find_map(|domain| hostname.strip_suffix(domain)?.strip_suffix('.'))?;
    if name.is_empty() || name.contains('.') {
        return None;
    }
    let header = if uuid::Uuid::parse_str(name).is_ok() {
        "x-worker-id"
    } else {
        "x-worker-name"
    };
    Some((header, name))
}

pub fn log_route(path: &str) -> Option<(uuid::Uuid, bool)> {
    let rest = path.strip_prefix("/api/v1/workers/")?;
    let (id, endpoint) = rest.split_once('/')?;
    let ws = match endpoint {
        "logs" => false,
        "ws-logs" => true,
        _ => return None,
    };
    Some((uuid::Uuid::parse_str(id).ok()?, ws))
}

pub fn tls_config()
-> Result<Option<Arc<rustls::ServerConfig>>, Box<dyn std::error::Error + Send + Sync>> {
    use rustls::pki_types::pem::PemObject;
    let cert = std::env::var("HTTP_TLS_CERTIFICATE")
        .ok()
        .filter(|s| !s.is_empty());
    let key = std::env::var("HTTP_TLS_KEY").ok().filter(|s| !s.is_empty());
    let (cert, key) = match (cert, key) {
        (None, None) => return Ok(None),
        (Some(cert), Some(key)) => (cert, key),
        _ => return Err("set both HTTP_TLS_CERTIFICATE and HTTP_TLS_KEY".into()),
    };
    let certs =
        rustls::pki_types::CertificateDer::pem_file_iter(cert)?.collect::<Result<Vec<_>, _>>()?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_file(key)?;
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Some(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inbound_filter_accepts_only_listed_networks() {
        let nets =
            parse_allowlist("# proxies\n192.0.2.0/24\n2001:db8::/32 # v6\n127.0.0.1\n").unwrap();
        for ip in ["192.0.2.8", "2001:db8::1", "127.0.0.1", "::ffff:192.0.2.8"] {
            assert!(allowed(Some(&nets), ip.parse().unwrap()));
        }
        assert!(!allowed(Some(&nets), "198.51.100.1".parse().unwrap()));
        assert!(!allowed(Some(&[]), "127.0.0.1".parse().unwrap()));
        assert!(allowed(None, "198.51.100.1".parse().unwrap()));
        assert!(parse_allowlist("192.0.2.0/999").is_err());
    }

    #[test]
    fn public_clients_cannot_select_workers_or_spoof_internal_requests() {
        let mut req = Request::builder()
            .header("host", "dash.example:443")
            .header("x-worker-id", "private")
            .header("x-openworkers-internal", "1")
            .header("x-openworkers-depth", "0")
            .header("x-forwarded-proto", "http")
            .header("x-forwarded-for", "127.0.0.1")
            .body(())
            .unwrap();
        normalize(&mut req, "192.0.2.1:12".parse().unwrap(), true);
        assert!(!req.headers().contains_key("x-worker-id"));
        assert!(!req.headers().contains_key("x-openworkers-internal"));
        assert_eq!(req.headers()["x-forwarded-proto"], "https");
        assert_eq!(req.headers()["x-forwarded-for"], "192.0.2.1");
        assert_eq!(host(req.headers()).as_deref(), Some("dash.example"));
        assert!(req.headers().contains_key("x-request-id"));
    }
    #[test]
    fn worker_domains_require_a_complete_suffix() {
        let domains = vec!["workers.rocks".into()];
        assert_eq!(
            worker_route("hello.workers.rocks", &domains),
            Some(("x-worker-name", "hello"))
        );
        assert!(worker_route("hello.workers.rocks.evil.test", &domains).is_none());
        assert!(worker_route("hello.notworkers.rocks", &domains).is_none());
        assert!(worker_route("nested.hello.workers.rocks", &domains).is_none());
        assert!(worker_route("workers.rocks", &domains).is_none());
        let id = uuid::Uuid::new_v4().to_string();
        let hostname = format!("{id}.workers.rocks");
        assert_eq!(
            worker_route(&hostname, &domains),
            Some(("x-worker-id", id.as_str()))
        );
    }

    #[test]
    fn http2_authority_supplies_host() {
        let mut req = Request::builder()
            .uri("https://hello.workers.rocks:8443/path")
            .body(())
            .unwrap();
        normalize(&mut req, "192.0.2.1:12".parse().unwrap(), true);
        assert_eq!(host(req.headers()).as_deref(), Some("hello.workers.rocks"));
        assert_eq!(req.uri().to_string(), "/path");
    }

    #[test]
    fn log_routes_do_not_match_other_worker_endpoints() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            log_route(&format!("/api/v1/workers/{id}/logs")),
            Some((id, false))
        );
        assert_eq!(
            log_route(&format!("/api/v1/workers/{id}/ws-logs")),
            Some((id, true))
        );
        assert!(log_route("/api/v1/workers/nope/logs").is_none());
        assert!(log_route(&format!("/api/v1/workers/{id}/logs/extra")).is_none());
    }
}
