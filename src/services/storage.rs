//! S3-compatible storage service using AWS v4 signing.
//!
//! This module implements S3-compatible storage operations (get, put, head, list, delete)
//! using AWS Signature Version 4 for authentication.
//!
//! ## Operations
//!
//! - `get` - Retrieve an object
//! - `put` - Store an object
//! - `head` - Get object metadata
//! - `list` - List objects with optional prefix
//! - `delete` - Remove an object
//! - `fetch` - HTTP-style fetch (returns streaming response)
//!
//! ## Security
//!
//! All paths are sanitized to prevent directory traversal attacks.
//! The sanitization handles multiple levels of URL encoding.

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use openworkers_core::{
    HttpMethod, HttpRequest, HttpResponse, RequestBody, ResponseBody, StorageOp, StorageResult,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::SystemTime;

use crate::store::StorageConfig;

/// Sanitize a path to prevent directory traversal attacks.
///
/// Security measures:
/// 1. URL-decodes repeatedly until stable (catches %252e, %25252e, etc.)
/// 2. Rejects null bytes (injection attacks)
/// 3. Rejects if `..` is present anywhere (directory traversal)
/// 4. Removes empty and `.` components
///
/// Returns None if the path is malicious or would escape the root.
fn sanitize_path(path: &str) -> Option<String> {
    use percent_encoding::percent_decode_str;

    // Decode repeatedly until the string stops changing.
    // This is necessary because percent_decode only decodes one level:
    // %252e%252e -> %2e%2e -> .. (needs 2 iterations)
    // Max 10 iterations prevents DoS from pathological inputs.
    let mut current = path.to_string();

    for _ in 0..10 {
        let decoded = percent_decode_str(&current).decode_utf8().ok()?;

        if decoded == current {
            break;
        }

        current = decoded.into_owned();
    }

    // Reject null bytes (can be used for injection attacks)
    if current.contains('\0') {
        return None;
    }

    // Reject any traversal pattern after full decoding
    if current.contains("..") {
        return None;
    }

    // Build clean path from remaining components
    let clean: String = current
        .split('/')
        .filter(|p: &&str| !p.is_empty() && *p != ".")
        .collect::<Vec<_>>()
        .join("/");

    Some(clean)
}

/// Build the actual S3/R2 URL for a binding fetch request.
///
/// URL structure: `{endpoint}/{bucket}/{prefix?}/{path}`
pub fn build_s3_url(config: &StorageConfig, url_or_path: &str) -> Result<String, String> {
    // Extract path from URL if it's a full URL (e.g., "http://localhost/_app/foo.js")
    let path = if url_or_path.starts_with("http://") || url_or_path.starts_with("https://") {
        url::Url::parse(url_or_path)
            .map(|u| u.path().to_string())
            .unwrap_or_else(|_| url_or_path.to_string())
    } else {
        url_or_path.to_string()
    };

    let full_path = match &config.prefix {
        Some(prefix) => {
            let sanitized = sanitize_path(&path)
                .ok_or_else(|| "Invalid path: traversal not allowed".to_string())?;
            format!("{}/{}", prefix.trim_end_matches('/'), sanitized)
        }
        None => path.trim_start_matches('/').to_string(),
    };

    Ok(format!(
        "{}/{}/{}",
        config.endpoint, config.bucket, full_path
    ))
}

/// Execute an S3 storage operation using AWS v4 signing
pub async fn execute_s3_operation(config: &StorageConfig, op: StorageOp) -> StorageResult {
    let region = config.region.as_deref().unwrap_or("auto");
    let credentials = Credentials::new(
        &config.access_key_id,
        &config.secret_access_key,
        None,
        None,
        "openworkers",
    );

    // Build the full key with optional prefix, always sanitizing to prevent traversal
    let build_full_key = |key: &str| -> Result<String, String> {
        let sanitized = sanitize_path(key)
            .ok_or_else(|| "Invalid key: directory traversal not allowed".to_string())?;

        match &config.prefix {
            Some(prefix) => Ok(format!("{}/{}", prefix.trim_end_matches('/'), sanitized)),
            None => Ok(sanitized),
        }
    };

    // Strip the config prefix from a key (inverse of build_full_key)
    let strip_prefix = |key: &str| -> String {
        match &config.prefix {
            Some(prefix) => {
                let prefix_with_slash = format!("{}/", prefix.trim_end_matches('/'));
                key.strip_prefix(&prefix_with_slash)
                    .unwrap_or(key)
                    .to_string()
            }
            None => key.to_string(),
        }
    };

    // Parse endpoint to get host
    let endpoint_url = match url::Url::parse(&config.endpoint) {
        Ok(u) => u,
        Err(e) => return StorageResult::Error(format!("Invalid endpoint URL: {}", e)),
    };
    let host = match endpoint_url.host_str() {
        Some(h) => h.to_string(),
        None => return StorageResult::Error("Endpoint URL has no host".to_string()),
    };

    match op {
        StorageOp::Get { key } => {
            let full_key = match build_full_key(&key) {
                Ok(k) => k,
                Err(e) => return StorageResult::Error(e),
            };
            let url = format!("{}/{}/{}", config.endpoint, config.bucket, full_key);

            match sign_and_execute("GET", &url, &host, region, &credentials, None).await {
                Ok((status, body)) => {
                    if status == 200 {
                        StorageResult::Body(Some(body))
                    } else if status == 404 {
                        StorageResult::Body(None)
                    } else {
                        StorageResult::Error(format!(
                            "S3 GET failed with status {}: {}",
                            status,
                            String::from_utf8_lossy(&body)
                        ))
                    }
                }
                Err(e) => StorageResult::Error(e),
            }
        }

        StorageOp::Fetch { key } => {
            let full_key = match build_full_key(&key) {
                Ok(k) => k,
                Err(e) => return StorageResult::Error(e),
            };
            let url = format!("{}/{}/{}", config.endpoint, config.bucket, full_key);

            match sign_and_execute_response("GET", &url, &host, region, &credentials, None).await {
                Ok(response) => StorageResult::Response(response),
                Err(e) => StorageResult::Error(e),
            }
        }

        StorageOp::Put { key, body } => {
            let full_key = match build_full_key(&key) {
                Ok(k) => k,
                Err(e) => return StorageResult::Error(e),
            };
            let url = format!("{}/{}/{}", config.endpoint, config.bucket, full_key);

            match sign_and_execute("PUT", &url, &host, region, &credentials, Some(body)).await {
                Ok((status, resp_body)) => {
                    if status == 200 || status == 201 {
                        StorageResult::Body(None)
                    } else {
                        StorageResult::Error(format!(
                            "S3 PUT failed with status {}: {}",
                            status,
                            String::from_utf8_lossy(&resp_body)
                        ))
                    }
                }
                Err(e) => StorageResult::Error(e),
            }
        }

        StorageOp::Head { key } => {
            let full_key = match build_full_key(&key) {
                Ok(k) => k,
                Err(e) => return StorageResult::Error(e),
            };
            let url = format!("{}/{}/{}", config.endpoint, config.bucket, full_key);

            match sign_and_execute_head(&url, &host, region, &credentials).await {
                Ok((status, size, etag)) => {
                    if status == 200 {
                        StorageResult::Head { size, etag }
                    } else if status == 404 {
                        StorageResult::Error("Object not found".to_string())
                    } else {
                        StorageResult::Error(format!("S3 HEAD failed with status {}", status))
                    }
                }
                Err(e) => StorageResult::Error(e),
            }
        }

        StorageOp::Delete { key } => {
            let full_key = match build_full_key(&key) {
                Ok(k) => k,
                Err(e) => return StorageResult::Error(e),
            };
            let url = format!("{}/{}/{}", config.endpoint, config.bucket, full_key);

            match sign_and_execute("DELETE", &url, &host, region, &credentials, None).await {
                Ok((status, resp_body)) => {
                    if status == 200 || status == 204 {
                        StorageResult::Body(None)
                    } else {
                        StorageResult::Error(format!(
                            "S3 DELETE failed with status {}: {}",
                            status,
                            String::from_utf8_lossy(&resp_body)
                        ))
                    }
                }
                Err(e) => StorageResult::Error(e),
            }
        }

        StorageOp::List { prefix, limit } => {
            // Build list URL with query params
            let list_prefix = match (&config.prefix, &prefix) {
                (Some(p), Some(user_p)) => {
                    format!(
                        "{}{}{}",
                        p.trim_end_matches('/'),
                        if user_p.is_empty() { "" } else { "/" },
                        user_p
                    )
                }
                (Some(p), None) => p.clone(),
                (None, Some(user_p)) => user_p.clone(),
                (None, None) => String::new(),
            };

            let mut url = format!("{}/{}?list-type=2", config.endpoint, config.bucket);

            if !list_prefix.is_empty() {
                url.push_str(&format!("&prefix={}", list_prefix));
            }

            if let Some(max) = limit {
                url.push_str(&format!("&max-keys={}", max));
            }

            match sign_and_execute("GET", &url, &host, region, &credentials, None).await {
                Ok((status, body)) => {
                    if status == 200 {
                        // Parse XML response to extract keys, stripping the config prefix
                        let body_str = String::from_utf8_lossy(&body);
                        let keys: Vec<String> = parse_list_response(&body_str)
                            .into_iter()
                            .map(|k| strip_prefix(&k))
                            .collect();
                        let truncated = body_str.contains("<IsTruncated>true</IsTruncated>");
                        StorageResult::List { keys, truncated }
                    } else {
                        StorageResult::Error(format!(
                            "S3 LIST failed with status {}: {}",
                            status,
                            String::from_utf8_lossy(&body)
                        ))
                    }
                }
                Err(e) => StorageResult::Error(e),
            }
        }
    }
}

/// Sign and execute an S3 request, returning status and body
async fn sign_and_execute(
    method: &str,
    url: &str,
    host: &str,
    region: &str,
    credentials: &Credentials,
    body: Option<Vec<u8>>,
) -> Result<(u16, Vec<u8>), String> {
    let response = sign_and_execute_raw(method, url, host, region, credentials, body).await?;
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read response body: {}", e))?
        .to_vec();
    Ok((status, body))
}

/// Sign and execute an S3 request, returning full HttpResponse
async fn sign_and_execute_response(
    method: &str,
    url: &str,
    host: &str,
    region: &str,
    credentials: &Credentials,
    body: Option<Vec<u8>>,
) -> Result<HttpResponse, String> {
    let response = sign_and_execute_raw(method, url, host, region, credentials, body).await?;
    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect();
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read response body: {}", e))?
        .to_vec();
    Ok(HttpResponse {
        status,
        headers,
        body: ResponseBody::Bytes(body.into()),
    })
}

/// Compute AWS v4 signature headers for an S3 request
fn compute_signature_headers(
    method: &str,
    uri: &str,
    host: &str,
    region: &str,
    credentials: &Credentials,
    body_bytes: &[u8],
) -> Result<(HashMap<String, String>, String), String> {
    let body_hash = hex::encode(Sha256::digest(body_bytes));

    let identity = credentials.clone().into();
    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("s3")
        .time(SystemTime::now())
        .settings(SigningSettings::default())
        .build()
        .map_err(|e| format!("Failed to build signing params: {}", e))?;

    let signable_body = if body_bytes.is_empty() {
        SignableBody::Bytes(&[])
    } else {
        SignableBody::Bytes(body_bytes)
    };

    let headers = vec![("host", host), ("x-amz-content-sha256", body_hash.as_str())];

    let signable_request = SignableRequest::new(method, uri, headers.into_iter(), signable_body)
        .map_err(|e| format!("Failed to create signable request: {}", e))?;

    let (signing_instructions, _) = sign(signable_request, &signing_params.into())
        .map_err(|e| format!("Failed to sign request: {}", e))?
        .into_parts();

    let mut signed_headers = HashMap::new();
    signed_headers.insert("host".to_string(), host.to_string());
    signed_headers.insert("x-amz-content-sha256".to_string(), body_hash.clone());

    for (name, value) in signing_instructions.headers() {
        signed_headers.insert(name.to_string(), value.to_string());
    }

    Ok((signed_headers, body_hash))
}

/// Sign and execute an S3 request, returning raw reqwest::Response
async fn sign_and_execute_raw(
    method: &str,
    url: &str,
    host: &str,
    region: &str,
    credentials: &Credentials,
    body: Option<Vec<u8>>,
) -> Result<reqwest::Response, String> {
    let body_bytes = body.as_deref().unwrap_or(&[]);

    let parsed_url = url::Url::parse(url).map_err(|e| e.to_string())?;
    let uri = parsed_url.path().to_string()
        + parsed_url
            .query()
            .map(|q| format!("?{}", q))
            .unwrap_or_default()
            .as_str();

    let (signed_headers, _) =
        compute_signature_headers(method, &uri, host, region, credentials, body_bytes)?;

    let client = reqwest::Client::new();
    let mut request_builder = match method {
        "GET" => client.get(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "HEAD" => client.head(url),
        _ => return Err(format!("Unsupported method: {}", method)),
    };

    for (name, value) in &signed_headers {
        request_builder = request_builder.header(name, value);
    }

    if method == "PUT"
        && let Some(b) = body
    {
        request_builder = request_builder.body(b);
    }

    request_builder
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))
}

/// Sign and execute an S3 HEAD request
async fn sign_and_execute_head(
    url: &str,
    host: &str,
    region: &str,
    credentials: &Credentials,
) -> Result<(u16, u64, Option<String>), String> {
    let response = sign_and_execute_raw("HEAD", url, host, region, credentials, None).await?;
    let status = response.status().as_u16();
    let size = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string());
    Ok((status, size, etag))
}

/// Parse S3 ListObjectsV2 XML response to extract keys
fn parse_list_response(xml: &str) -> Vec<String> {
    let mut keys = Vec::new();

    // Simple XML parsing - look for <Key>...</Key> tags
    for part in xml.split("<Key>") {
        if let Some(end) = part.find("</Key>") {
            keys.push(part[..end].to_string());
        }
    }

    keys
}

/// Sign an S3/R2 request using AWS Signature V4.
///
/// R2 uses AWS Signature V4 for authentication (not Bearer tokens).
/// The region is always "auto" for R2.
pub fn sign_s3_request(
    request: &HttpRequest,
    access_key_id: &str,
    secret_access_key: &str,
    _bucket: &str,
) -> Result<HashMap<String, String>, String> {
    let url = url::Url::parse(&request.url).map_err(|e| format!("Invalid URL: {}", e))?;
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    let method = match request.method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Head => "HEAD",
        HttpMethod::Options => "OPTIONS",
    };

    let credentials = Credentials::new(access_key_id, secret_access_key, None, None, "openworkers");

    let body_bytes: &[u8] = match &request.body {
        RequestBody::None | RequestBody::Stream(_) => &[],
        RequestBody::Bytes(b) => b,
    };

    let (signed_headers, _) =
        compute_signature_headers(method, &request.url, host, "auto", &credentials, body_bytes)?;

    Ok(signed_headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_path_valid() {
        // Normal paths
        assert_eq!(sanitize_path("hello.txt"), Some("hello.txt".to_string()));
        assert_eq!(
            sanitize_path("folder/file.txt"),
            Some("folder/file.txt".to_string())
        );
        assert_eq!(
            sanitize_path("/folder/file.txt"),
            Some("folder/file.txt".to_string())
        );
        assert_eq!(
            sanitize_path("a/b/c/d.txt"),
            Some("a/b/c/d.txt".to_string())
        );

        // Single dot is removed
        assert_eq!(
            sanitize_path("./folder/file.txt"),
            Some("folder/file.txt".to_string())
        );
        assert_eq!(
            sanitize_path("folder/./file.txt"),
            Some("folder/file.txt".to_string())
        );

        // URL-encoded valid characters
        assert_eq!(
            sanitize_path("hello%20world.txt"),
            Some("hello world.txt".to_string())
        );
    }

    #[test]
    fn test_sanitize_path_traversal_plain() {
        // Plain traversal
        assert_eq!(sanitize_path(".."), None);
        assert_eq!(sanitize_path("../etc/passwd"), None);
        assert_eq!(sanitize_path("folder/../secret"), None);
        assert_eq!(sanitize_path("a/b/../../c"), None);
    }

    #[test]
    fn test_sanitize_path_traversal_url_encoded() {
        // Single URL-encoded (%2e = .)
        assert_eq!(sanitize_path("%2e%2e"), None);
        assert_eq!(sanitize_path("%2e%2e/etc/passwd"), None);
        assert_eq!(sanitize_path("folder/%2e%2e/secret"), None);

        // Mixed case
        assert_eq!(sanitize_path("%2E%2E"), None);
        assert_eq!(sanitize_path("%2e%2E"), None);
    }

    #[test]
    fn test_sanitize_path_traversal_double_encoded() {
        // Double URL-encoded (%252e decodes to %2e, then to .)
        assert_eq!(sanitize_path("%252e%252e"), None);
        assert_eq!(sanitize_path("%252e%252e/etc/passwd"), None);
        assert_eq!(sanitize_path("folder/%252e%252e/secret"), None);
    }

    #[test]
    fn test_sanitize_path_traversal_triple_encoded() {
        // Triple URL-encoded (%25252e decodes to %252e, then %2e, then .)
        assert_eq!(sanitize_path("%25252e%25252e"), None);
        assert_eq!(sanitize_path("%25252e%25252e/etc/passwd"), None);
    }

    #[test]
    fn test_sanitize_path_null_byte() {
        // Null byte injection
        assert_eq!(sanitize_path("file.txt\0.jpg"), None);
        assert_eq!(sanitize_path("file%00.txt"), None);
        assert_eq!(sanitize_path("%00"), None);
    }

    #[test]
    fn test_sanitize_path_invalid_utf8() {
        // Invalid UTF-8 sequences should return None
        assert_eq!(sanitize_path("%ff%fe"), None);
    }

    // ============================================================================
    // build_s3_url tests
    // ============================================================================

    fn make_config(endpoint: &str, bucket: &str, prefix: Option<&str>) -> StorageConfig {
        StorageConfig {
            id: "test-config-id".to_string(),
            endpoint: endpoint.to_string(),
            bucket: bucket.to_string(),
            prefix: prefix.map(|s| s.to_string()),
            access_key_id: "test-key".to_string(),
            secret_access_key: "test-secret".to_string(),
            region: Some("us-east-1".to_string()),
            public_url: None,
        }
    }

    #[test]
    fn test_build_s3_url_simple_path() {
        let config = make_config("https://s3.example.com", "my-bucket", None);
        let url = build_s3_url(&config, "/file.txt").unwrap();
        assert_eq!(url, "https://s3.example.com/my-bucket/file.txt");
    }

    #[test]
    fn test_build_s3_url_with_prefix() {
        let config = make_config("https://s3.example.com", "my-bucket", Some("assets"));
        let url = build_s3_url(&config, "/file.txt").unwrap();
        assert_eq!(url, "https://s3.example.com/my-bucket/assets/file.txt");
    }

    #[test]
    fn test_build_s3_url_prefix_trailing_slash() {
        let config = make_config("https://s3.example.com", "my-bucket", Some("assets/"));
        let url = build_s3_url(&config, "/file.txt").unwrap();
        assert_eq!(url, "https://s3.example.com/my-bucket/assets/file.txt");
    }

    #[test]
    fn test_build_s3_url_nested_path() {
        let config = make_config("https://s3.example.com", "my-bucket", Some("v1"));
        let url = build_s3_url(&config, "/images/logo.png").unwrap();
        assert_eq!(url, "https://s3.example.com/my-bucket/v1/images/logo.png");
    }

    #[test]
    fn test_build_s3_url_from_full_url() {
        let config = make_config("https://s3.example.com", "my-bucket", None);
        let url = build_s3_url(&config, "http://localhost:8080/_app/file.js").unwrap();
        assert_eq!(url, "https://s3.example.com/my-bucket/_app/file.js");
    }

    #[test]
    fn test_build_s3_url_traversal_rejected() {
        let config = make_config("https://s3.example.com", "my-bucket", Some("safe"));
        let result = build_s3_url(&config, "/../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("traversal"));
    }

    #[test]
    fn test_build_s3_url_encoded_traversal_rejected() {
        let config = make_config("https://s3.example.com", "my-bucket", Some("safe"));
        let result = build_s3_url(&config, "/%2e%2e/etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn test_build_s3_url_no_prefix_allows_any_path() {
        // Without prefix, paths are not sanitized (direct S3 access)
        let config = make_config("https://s3.example.com", "my-bucket", None);
        let url = build_s3_url(&config, "/any/path/here.txt").unwrap();
        assert_eq!(url, "https://s3.example.com/my-bucket/any/path/here.txt");
    }

    #[test]
    fn test_build_s3_url_empty_path() {
        let config = make_config("https://s3.example.com", "my-bucket", Some("prefix"));
        let url = build_s3_url(&config, "/").unwrap();
        assert_eq!(url, "https://s3.example.com/my-bucket/prefix/");
    }

    #[test]
    fn test_build_s3_url_cloudflare_r2() {
        let config = make_config(
            "https://account-id.r2.cloudflarestorage.com",
            "workers-assets",
            Some("static"),
        );
        let url = build_s3_url(&config, "/css/main.css").unwrap();
        assert_eq!(
            url,
            "https://account-id.r2.cloudflarestorage.com/workers-assets/static/css/main.css"
        );
    }
}
