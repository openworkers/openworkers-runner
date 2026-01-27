//! S3 storage operations using AWS v4 signing
//!
//! This module implements S3-compatible storage operations (get, put, head, list, delete)
//! using AWS Signature Version 4 for authentication.

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use openworkers_core::{StorageOp, StorageResult};
use sha2::{Digest, Sha256};
use std::time::SystemTime;

use crate::store::StorageConfig;

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

    // Build the full key with optional prefix
    let build_full_key = |key: &str| -> String {
        match &config.prefix {
            Some(prefix) => format!("{}/{}", prefix.trim_end_matches('/'), key),
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
            let full_key = build_full_key(&key);
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

        StorageOp::Put { key, body } => {
            let full_key = build_full_key(&key);
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
            let full_key = build_full_key(&key);
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
            let full_key = build_full_key(&key);
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
                        // Parse XML response to extract keys
                        let body_str = String::from_utf8_lossy(&body);
                        let keys = parse_list_response(&body_str);
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

/// Sign and execute an S3 request (GET/PUT/DELETE)
async fn sign_and_execute(
    method: &str,
    url: &str,
    host: &str,
    region: &str,
    credentials: &Credentials,
    body: Option<Vec<u8>>,
) -> Result<(u16, Vec<u8>), String> {
    let body_bytes = body.as_deref().unwrap_or(&[]);
    let body_hash = hex::encode(Sha256::digest(body_bytes));

    let parsed_url = url::Url::parse(url).map_err(|e| e.to_string())?;
    let uri = parsed_url.path().to_string()
        + parsed_url
            .query()
            .map(|q| format!("?{}", q))
            .unwrap_or_default()
            .as_str();

    // Build signing params
    let identity = credentials.clone().into();
    let signing_settings = SigningSettings::default();
    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("s3")
        .time(SystemTime::now())
        .settings(signing_settings)
        .build()
        .map_err(|e| format!("Failed to build signing params: {}", e))?;

    // Create signable request
    let signable_body = if body_bytes.is_empty() {
        SignableBody::Bytes(&[])
    } else {
        SignableBody::Bytes(body_bytes)
    };

    let content_hash_header = body_hash.clone();
    let headers = vec![
        ("host", host),
        ("x-amz-content-sha256", &content_hash_header),
    ];

    let signable_request = SignableRequest::new(method, &uri, headers.into_iter(), signable_body)
        .map_err(|e| format!("Failed to create signable request: {}", e))?;

    // Sign the request
    let (signing_instructions, _signature) = sign(signable_request, &signing_params.into())
        .map_err(|e| format!("Failed to sign request: {}", e))?
        .into_parts();

    // Build reqwest request with signed headers
    let client = reqwest::Client::new();
    let mut request_builder = match method {
        "GET" => client.get(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        _ => return Err(format!("Unsupported method: {}", method)),
    };

    // Apply signing instructions (headers)
    for (name, value) in signing_instructions.headers() {
        request_builder = request_builder.header(name, value.as_bytes());
    }
    request_builder = request_builder.header("x-amz-content-sha256", &body_hash);

    // Add body for PUT
    if method == "PUT"
        && let Some(b) = body
    {
        request_builder = request_builder.body(b);
    }

    let response = request_builder
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;

    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read response body: {}", e))?
        .to_vec();

    Ok((status, body))
}

/// Sign and execute an S3 HEAD request
async fn sign_and_execute_head(
    url: &str,
    host: &str,
    region: &str,
    credentials: &Credentials,
) -> Result<(u16, u64, Option<String>), String> {
    let body_hash = hex::encode(Sha256::digest([]));
    let parsed_url = url::Url::parse(url).map_err(|e| e.to_string())?;
    let uri = parsed_url.path().to_string();

    let identity = credentials.clone().into();
    let signing_settings = SigningSettings::default();
    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("s3")
        .time(SystemTime::now())
        .settings(signing_settings)
        .build()
        .map_err(|e| format!("Failed to build signing params: {}", e))?;

    let headers = vec![("host", host), ("x-amz-content-sha256", &body_hash)];
    let signable_request =
        SignableRequest::new("HEAD", &uri, headers.into_iter(), SignableBody::Bytes(&[]))
            .map_err(|e| format!("Failed to create signable request: {}", e))?;

    let (signing_instructions, _) = sign(signable_request, &signing_params.into())
        .map_err(|e| format!("Failed to sign request: {}", e))?
        .into_parts();

    let client = reqwest::Client::new();
    let mut request_builder = client.head(url);

    for (name, value) in signing_instructions.headers() {
        request_builder = request_builder.header(name, value.as_bytes());
    }
    request_builder = request_builder.header("x-amz-content-sha256", &body_hash);

    let response = request_builder
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;

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
