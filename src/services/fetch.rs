//! HTTP fetch service with internal worker routing.
//!
//! This module provides HTTP request functionality with:
//! - Thread-local connection pooling
//! - Internal routing for worker-to-worker calls
//! - Streaming response support
//!
//! ## Internal Routing
//!
//! URLs matching `*.{WORKER_DOMAINS}` are routed internally to avoid
//! DNS lookup and external network hops. For example, if WORKER_DOMAINS
//! contains "workers.rocks", a fetch to `https://my-app.workers.rocks/api`
//! will be routed to `http://127.0.0.1:8080/api` with an `x-worker-name` header.

use bytes::Bytes;
use once_cell::sync::Lazy;
use openworkers_core::{HttpMethod, HttpRequest, HttpResponse, RequestBody, ResponseBody};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::ops::OperationsStats;

/// Worker domains for internal routing (e.g., "workers.rocks,workers.dev.localhost")
/// URLs matching `*.{domain}` will be routed internally instead of going through DNS.
pub static WORKER_DOMAINS: Lazy<Vec<String>> = Lazy::new(|| {
    std::env::var("WORKER_DOMAINS")
        .map(|s| {
            s.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_else(|_| Vec::new())
});

/// Generate a unique request ID (32 chars total: prefix_hex)
pub fn generate_request_id(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let hex_len = 31 - prefix.len();
    format!("{}_{:0width$x}", prefix, nanos, width = hex_len)
}

// Thread-local HTTP client with connection pooling.
//
// Each worker thread gets its own client to avoid cross-runtime issues.
// Configuration:
// - pool_max_idle_per_host: Configurable via HTTP_POOL_MAX_IDLE_PER_HOST env var (default: 100)
// - pool_idle_timeout: 90s (keep connections warm)
// - connect_timeout: 5s (DNS + TCP handshake)
// - timeout: 30s (total request)
thread_local! {
    static HTTP_CLIENT: once_cell::unsync::Lazy<reqwest::Client> = once_cell::unsync::Lazy::new(|| {
        let pool_size = std::env::var("HTTP_POOL_MAX_IDLE_PER_HOST")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(100);

        reqwest::Client::builder()
            .pool_max_idle_per_host(pool_size)
            .pool_idle_timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client")
    });
}

/// Check if a request should be routed internally to another worker.
///
/// Patterns are configured via WORKER_DOMAINS env var (e.g., "workers.rocks,workers.dev.localhost")
/// URLs matching `*.{domain}` will be routed internally.
///
/// Returns a modified request with internal routing if matched.
pub fn try_internal_worker_route(request: &HttpRequest) -> Option<HttpRequest> {
    let url = url::Url::parse(&request.url).ok()?;
    let host = url.host_str()?;

    // Check for internal worker patterns from WORKER_DOMAINS
    let worker_name = WORKER_DOMAINS.iter().find_map(|domain| {
        let suffix = format!(".{}", domain);
        host.strip_suffix(&suffix)
    })?;

    // Build internal URL
    let path_and_query = match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    };
    let internal_url = format!("http://127.0.0.1:8080{}", path_and_query);

    // Create request with x-worker-name header
    let mut headers = request.headers.clone();
    headers.insert("x-worker-name".to_string(), worker_name.to_string());
    headers.insert("x-request-id".to_string(), generate_request_id("internal"));

    // Can't clone a streaming body, so internal routing is not supported for streams
    let body = match &request.body {
        RequestBody::None => RequestBody::None,
        RequestBody::Bytes(b) => RequestBody::Bytes(b.clone()),
        RequestBody::Stream(_) => return None,
    };

    Some(HttpRequest {
        url: internal_url,
        method: request.method.clone(),
        headers,
        body,
    })
}

/// Execute an HTTP request with streaming response.
///
/// This is the core HTTP function used by both direct fetch and binding fetches.
/// Returns a streaming response that can be consumed chunk by chunk.
pub async fn do_fetch(
    request: HttpRequest,
    stats: &OperationsStats,
    extra_headers: Option<&HashMap<String, String>>,
) -> Result<HttpResponse, String> {
    use std::sync::atomic::Ordering;

    // Use thread-local HTTP client (created in this thread's runtime)
    // Clone is cheap - reqwest::Client is an Arc internally
    let client = HTTP_CLIENT.with(|c| (**c).clone());

    // Prepare request builder
    let mut req_builder = match request.method {
        HttpMethod::Get => client.get(&request.url),
        HttpMethod::Post => client.post(&request.url),
        HttpMethod::Put => client.put(&request.url),
        HttpMethod::Delete => client.delete(&request.url),
        HttpMethod::Patch => client.patch(&request.url),
        HttpMethod::Head => client.head(&request.url),
        HttpMethod::Options => {
            return Err("OPTIONS method not yet supported".to_string());
        }
    };

    // Add headers from request
    for (key, value) in &request.headers {
        req_builder = req_builder.header(key, value);
    }

    // Add extra headers if provided
    if let Some(headers) = extra_headers {
        for (key, value) in headers {
            req_builder = req_builder.header(key, value);
        }
    }

    // Add body if present
    if let RequestBody::Bytes(body) = &request.body {
        stats
            .fetch_bytes_out
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        req_builder = req_builder.body(body.clone());
    }

    // Execute request
    let response = req_builder
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    let status = response.status().as_u16();

    // Collect headers
    let mut headers = Vec::new();

    for (key, value) in response.headers() {
        if let Ok(value_str) = value.to_str() {
            headers.push((key.to_string(), value_str.to_string()));
        }
    }

    // Stream the body
    let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
    let stats_clone = std::sync::Arc::new(OperationsStats::new());

    // Copy stats for the spawned task
    let fetch_bytes_in = stats.fetch_bytes_in.load(Ordering::Relaxed);
    stats_clone
        .fetch_bytes_in
        .store(fetch_bytes_in, Ordering::Relaxed);

    // Use tokio::spawn() since worker threads now use multi_thread runtime.
    // This allows the stream task to outlive the worker's LocalSet.
    // The stream continues on the tokio runtime even after worker completes.
    tokio::task::spawn(async move {
        use futures::StreamExt;
        let mut stream = response.bytes_stream();

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    stats_clone
                        .fetch_bytes_in
                        .fetch_add(chunk.len() as u64, Ordering::Relaxed);

                    if tx.send(Ok(chunk)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                    break;
                }
            }
        }
    });

    Ok(HttpResponse {
        status,
        headers,
        body: ResponseBody::Stream(rx),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_request_id_format() {
        let id = generate_request_id("test");
        assert!(
            id.starts_with("test_"),
            "ID should start with prefix_: {}",
            id
        );
        assert_eq!(id.len(), 32, "ID should be 32 chars: {}", id);
    }

    #[test]
    fn test_generate_request_id_unique() {
        let id1 = generate_request_id("test");
        // Small sleep to ensure different nanosecond timestamp
        std::thread::sleep(std::time::Duration::from_nanos(100));
        let id2 = generate_request_id("test");
        assert_ne!(id1, id2, "IDs should be unique");
    }

    #[test]
    fn test_internal_route_no_match_external() {
        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "https://example.com/api".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let routed = try_internal_worker_route(&request);
        assert!(routed.is_none(), "Should not route external URLs");
    }

    #[test]
    fn test_internal_route_with_configured_domain() {
        if WORKER_DOMAINS.is_empty() {
            eprintln!("Skipping: WORKER_DOMAINS not set");
            return;
        }

        let domain = &WORKER_DOMAINS[0];
        let request = HttpRequest {
            method: HttpMethod::Get,
            url: format!("https://my-worker.{}/api/test", domain),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let routed = try_internal_worker_route(&request);
        assert!(routed.is_some(), "Should route configured domain");

        let routed = routed.unwrap();
        assert_eq!(routed.url, "http://127.0.0.1:8080/api/test");
        assert_eq!(
            routed.headers.get("x-worker-name"),
            Some(&"my-worker".to_string())
        );
    }
}
