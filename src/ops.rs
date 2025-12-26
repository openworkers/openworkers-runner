//! Operations implementation for the runner
//!
//! This module implements the OperationsHandler trait from openworkers-core,
//! providing fetch with stats tracking and binding support.
//!
//! ## Flow
//!
//! ```text
//! Worker JS code                    Runtime                     Runner (this module)
//! ─────────────────────────────────────────────────────────────────────────────────
//! fetch("https://api.com")    →    Operation::Fetch        →    handle_fetch()
//!                                                               └─ direct HTTP request
//!
//! env.ASSETS.fetch("/img.png") →   Operation::BindingFetch →    handle_binding_fetch()
//!                                  binding="ASSETS"             └─ lookup config
//!                                                               └─ inject auth token
//!                                                               └─ build S3/R2 URL
//!                                                               └─ HTTP request
//!
//! console.log("hello")        →    Operation::Log          →    handle_log()
//!                                                               └─ send to log channel
//! ```
//!
//! ## Security
//!
//! The worker never sees the actual credentials (tokens, access keys).
//! It only knows the binding NAME (e.g., "ASSETS"). The runner looks up
//! the config and injects auth headers transparently.

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use bytes::Bytes;
use openworkers_core::{
    HttpMethod, HttpRequest, HttpResponse, KvOp, KvResult, LogEvent, LogLevel, OpFuture,
    OperationsHandler, RequestBody, ResponseBody, StorageOp, StorageResult,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use tokio::sync::mpsc;

use crate::store::{AssetsConfig, Binding, KvConfig, StorageConfig};

/// Stats tracked for each worker
#[derive(Debug, Default)]
pub struct OperationsStats {
    pub fetch_count: AtomicU64,
    pub fetch_bytes_in: AtomicU64,
    pub fetch_bytes_out: AtomicU64,
    pub log_count: AtomicU64,
}

impl OperationsStats {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Log message sender (sends LogEvent to the log handler)
pub type LogTx = std::sync::mpsc::Sender<LogEvent>;

/// Binding configs indexed by binding name
#[derive(Debug, Default, Clone)]
pub struct BindingConfigs {
    pub assets: HashMap<String, AssetsConfig>,
    pub storage: HashMap<String, StorageConfig>,
    pub kv: HashMap<String, KvConfig>,
}

impl BindingConfigs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from a list of bindings
    pub fn from_bindings(bindings: Vec<Binding>) -> Self {
        let mut configs = Self::new();

        for binding in bindings {
            match binding {
                Binding::Assets { key, config } => {
                    configs.assets.insert(key, config);
                }
                Binding::Storage { key, config } => {
                    configs.storage.insert(key, config);
                }
                Binding::Kv { key, config } => {
                    configs.kv.insert(key, config);
                }
                // Var and Secret are not resource bindings
                Binding::Var { .. } | Binding::Secret { .. } => {}
            }
        }

        configs
    }
}

/// Database pool type alias
pub type DbPool = sqlx::Pool<sqlx::Postgres>;

/// Runner's implementation of OperationsHandler
///
/// Implements fetch via reqwest, bindings via config lookup, and logs via channel.
pub struct RunnerOperations {
    /// Stats for this worker
    pub stats: Arc<OperationsStats>,
    /// User ID for logging/quotas
    pub user_id: Option<String>,
    /// Worker ID for logging/quotas
    pub worker_id: Option<String>,
    /// Log sender (optional - if None, uses default stderr)
    pub log_tx: Option<LogTx>,
    /// Binding configs for this worker
    pub bindings: BindingConfigs,
    /// Database pool for KV operations
    pub db_pool: Option<DbPool>,
}

impl RunnerOperations {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(OperationsStats::new()),
            user_id: None,
            worker_id: None,
            log_tx: None,
            bindings: BindingConfigs::new(),
            db_pool: None,
        }
    }

    pub fn with_user_id(mut self, user_id: String) -> Self {
        self.user_id = Some(user_id);
        self
    }

    pub fn with_worker_id(mut self, worker_id: String) -> Self {
        self.worker_id = Some(worker_id);
        self
    }

    pub fn with_log_tx(mut self, log_tx: LogTx) -> Self {
        self.log_tx = Some(log_tx);
        self
    }

    pub fn with_bindings(mut self, bindings: Vec<Binding>) -> Self {
        self.bindings = BindingConfigs::from_bindings(bindings);
        self
    }

    pub fn with_db_pool(mut self, pool: DbPool) -> Self {
        self.db_pool = Some(pool);
        self
    }
}

impl Default for RunnerOperations {
    fn default() -> Self {
        Self::new()
    }
}

impl OperationsHandler for RunnerOperations {
    /// Handle direct fetch: `fetch("https://example.com")`
    ///
    /// This is a pass-through fetch with no auth modification.
    /// Used when worker code calls the global `fetch()` function.
    fn handle_fetch(&self, request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move {
            self.stats.fetch_count.fetch_add(1, Ordering::Relaxed);

            log::debug!(
                "[ops] fetch {} {} (user={:?}, worker={:?})",
                request.method,
                request.url,
                self.user_id,
                self.worker_id
            );

            do_fetch(request, &self.stats, None).await
        })
    }

    /// Handle binding fetch: `env.ASSETS.fetch("/path/to/file")`
    ///
    /// This is called when worker code uses a binding's fetch method.
    /// The runner:
    /// 1. Looks up the binding config by name
    /// 2. Builds the actual S3/R2 URL (endpoint + bucket + prefix + path)
    /// 3. Injects auth headers (Bearer token for R2, AWS sig for S3)
    /// 4. Executes the HTTP request
    ///
    /// The worker never sees the credentials - only the binding name.
    fn handle_binding_fetch(
        &self,
        binding: &str,
        request: HttpRequest,
    ) -> OpFuture<'_, Result<HttpResponse, String>> {
        // Check if binding exists in assets configs
        if let Some(assets_config) = self.bindings.assets.get(binding) {
            let config = assets_config.clone();
            let binding_name = binding.to_string();

            return Box::pin(async move {
                self.stats.fetch_count.fetch_add(1, Ordering::Relaxed);

                log::debug!(
                    "[ops] binding_fetch {} {} {} (user={:?}, worker={:?})",
                    binding_name,
                    request.method,
                    request.url,
                    self.user_id,
                    self.worker_id
                );

                // Build the actual S3/R2 URL
                let url = build_assets_url(&config, &request.url)?;

                // Create modified request with AWS signature
                let auth_request = HttpRequest {
                    url: url.clone(),
                    method: request.method,
                    headers: request.headers.clone(),
                    body: request.body,
                };

                // Sign the request with AWS Signature V4
                let signed_headers = sign_s3_request(
                    &auth_request,
                    &config.access_key_id,
                    &config.secret_access_key,
                    &config.bucket,
                )?;

                do_fetch(auth_request, &self.stats, Some(&signed_headers)).await
            });
        }

        if let Some(storage_config) = self.bindings.storage.get(binding) {
            let config = storage_config.clone();
            let binding_name = binding.to_string();

            return Box::pin(async move {
                self.stats.fetch_count.fetch_add(1, Ordering::Relaxed);

                log::debug!(
                    "[ops] binding_fetch storage {} {} {} (user={:?}, worker={:?})",
                    binding_name,
                    request.method,
                    request.url,
                    self.user_id,
                    self.worker_id
                );

                // For S3, we need AWS signature - for now return error
                // TODO: Implement AWS S3 signing
                Err(format!(
                    "Storage binding '{}' not yet implemented (requires S3 signing for {})",
                    binding_name, config.endpoint
                ))
            });
        }

        if self.bindings.kv.contains_key(binding) {
            let binding_name = binding.to_string();

            return Box::pin(async move {
                Err(format!(
                    "KV binding '{}' does not support fetch - use kv.get/put/delete",
                    binding_name
                ))
            });
        }

        // Binding not found
        let binding_name = binding.to_string();
        Box::pin(async move { Err(format!("Binding '{}' not configured", binding_name)) })
    }

    /// Handle storage operation: `env.STORAGE.get("key")`, `.put()`, `.head()`, `.list()`, `.delete()`
    ///
    /// Currently returns "not implemented" for all operations.
    fn handle_binding_storage(&self, binding: &str, op: StorageOp) -> OpFuture<'_, StorageResult> {
        let binding_name = binding.to_string();

        // Check if binding exists in storage configs
        if self.bindings.storage.contains_key(binding) {
            let op_name = match &op {
                StorageOp::Get { key } => format!("get({})", key),
                StorageOp::Put { key, .. } => format!("put({})", key),
                StorageOp::Head { key } => format!("head({})", key),
                StorageOp::List { prefix, .. } => {
                    format!("list(prefix={:?})", prefix)
                }
                StorageOp::Delete { key } => format!("delete({})", key),
            };

            return Box::pin(async move {
                log::debug!(
                    "[ops] storage {} {} (not yet implemented)",
                    binding_name,
                    op_name
                );

                StorageResult::Error(format!(
                    "Storage binding '{}' operation {} not yet implemented",
                    binding_name, op_name
                ))
            });
        }

        // Binding not found
        Box::pin(async move {
            StorageResult::Error(format!("Storage binding '{}' not configured", binding_name))
        })
    }

    /// Handle KV operation: `env.KV.get("key")`, `.put()`, `.delete()`
    fn handle_binding_kv(&self, binding: &str, op: KvOp) -> OpFuture<'_, KvResult> {
        let binding_name = binding.to_string();

        // Check if binding exists in kv configs
        let config = match self.bindings.kv.get(binding) {
            Some(c) => c.clone(),
            None => {
                return Box::pin(async move {
                    KvResult::Error(format!("KV binding '{}' not configured", binding_name))
                });
            }
        };

        // Check if we have a database pool
        let pool = match &self.db_pool {
            Some(p) => p.clone(),
            None => {
                return Box::pin(async move {
                    KvResult::Error("KV operations require database connection".to_string())
                });
            }
        };

        let namespace_id = config.id.clone();

        Box::pin(async move {
            match op {
                KvOp::Get { key } => {
                    let result = sqlx::query_scalar::<_, String>(
                        r#"
                        SELECT value FROM kv_data
                        WHERE namespace_id = $1::uuid AND key = $2
                        AND (expires_at IS NULL OR expires_at > NOW())
                        "#,
                    )
                    .bind(&namespace_id)
                    .bind(&key)
                    .fetch_optional(&pool)
                    .await;

                    match result {
                        Ok(value) => KvResult::Value(value),
                        Err(e) => {
                            log::error!("[ops] kv get error: {}", e);
                            KvResult::Error(format!("KV get failed: {}", e))
                        }
                    }
                }

                KvOp::Put {
                    key,
                    value,
                    expires_in,
                } => {
                    // Calculate expires_at from expires_in (seconds from now)
                    let result = if let Some(ttl) = expires_in {
                        sqlx::query(
                            r#"
                            INSERT INTO kv_data (namespace_id, key, value, expires_at, updated_at)
                            VALUES ($1::uuid, $2, $3, NOW() + $4 * INTERVAL '1 second', NOW())
                            ON CONFLICT (namespace_id, key)
                            DO UPDATE SET value = $3, expires_at = NOW() + $4 * INTERVAL '1 second', updated_at = NOW()
                            "#,
                        )
                        .bind(&namespace_id)
                        .bind(&key)
                        .bind(&value)
                        .bind(ttl as i64)
                        .execute(&pool)
                        .await
                    } else {
                        sqlx::query(
                            r#"
                            INSERT INTO kv_data (namespace_id, key, value, expires_at, updated_at)
                            VALUES ($1::uuid, $2, $3, NULL, NOW())
                            ON CONFLICT (namespace_id, key)
                            DO UPDATE SET value = $3, expires_at = NULL, updated_at = NOW()
                            "#,
                        )
                        .bind(&namespace_id)
                        .bind(&key)
                        .bind(&value)
                        .execute(&pool)
                        .await
                    };

                    match result {
                        Ok(_) => KvResult::Ok,
                        Err(e) => {
                            log::error!("[ops] kv put error: {}", e);
                            KvResult::Error(format!("KV put failed: {}", e))
                        }
                    }
                }

                KvOp::Delete { key } => {
                    let result = sqlx::query(
                        r#"
                        DELETE FROM kv_data
                        WHERE namespace_id = $1::uuid AND key = $2
                        "#,
                    )
                    .bind(&namespace_id)
                    .bind(&key)
                    .execute(&pool)
                    .await;

                    match result {
                        Ok(_) => KvResult::Ok,
                        Err(e) => {
                            log::error!("[ops] kv delete error: {}", e);
                            KvResult::Error(format!("KV delete failed: {}", e))
                        }
                    }
                }

                KvOp::List { prefix, limit } => {
                    let limit_val = limit.unwrap_or(1000) as i64;

                    let result = if let Some(prefix) = prefix {
                        let pattern = format!("{}%", prefix);
                        sqlx::query_scalar::<_, String>(
                            r#"
                            SELECT key FROM kv_data
                            WHERE namespace_id = $1::uuid
                            AND key LIKE $2
                            AND (expires_at IS NULL OR expires_at > NOW())
                            ORDER BY key
                            LIMIT $3
                            "#,
                        )
                        .bind(&namespace_id)
                        .bind(&pattern)
                        .bind(limit_val)
                        .fetch_all(&pool)
                        .await
                    } else {
                        sqlx::query_scalar::<_, String>(
                            r#"
                            SELECT key FROM kv_data
                            WHERE namespace_id = $1::uuid
                            AND (expires_at IS NULL OR expires_at > NOW())
                            ORDER BY key
                            LIMIT $2
                            "#,
                        )
                        .bind(&namespace_id)
                        .bind(limit_val)
                        .fetch_all(&pool)
                        .await
                    };

                    match result {
                        Ok(keys) => KvResult::Keys(keys),
                        Err(e) => {
                            log::error!("[ops] kv list error: {}", e);
                            KvResult::Error(format!("KV list failed: {}", e))
                        }
                    }
                }
            }
        })
    }

    /// Handle log: `console.log("message")`
    ///
    /// Sends log to the log channel (for collection) and also logs locally.
    fn handle_log(&self, level: LogLevel, message: String) {
        self.stats.log_count.fetch_add(1, Ordering::Relaxed);

        // Send to log channel if available (for log collection/storage)
        if let Some(ref tx) = self.log_tx {
            let _ = tx.send(LogEvent {
                level,
                message: message.clone(),
            });
        }

        // Also log via the log crate for debugging
        match level {
            LogLevel::Error => log::error!("[worker] {}", message),
            LogLevel::Warn => log::warn!("[worker] {}", message),
            LogLevel::Info | LogLevel::Log => log::info!("[worker] {}", message),
            LogLevel::Debug | LogLevel::Trace => log::debug!("[worker] {}", message),
        }
    }
}

/// Build the actual S3/R2 URL for an assets binding request.
///
/// URL structure: `{endpoint}/{bucket}/{prefix?}/{path}`
///
/// Two modes supported:
///
/// 1. **Shared S3** (endpoint=NULL, prefix=Some):
///    Platform-provisioned S3/R2 with prefix isolation. Each binding gets an
///    allocated prefix within the shared bucket, with a token scoped to that prefix.
///    Both AWS S3 (via IAM policies/Access Points) and Cloudflare R2 support
///    prefix-scoped tokens for secure multi-tenant isolation.
///    → `https://platform-s3.../shared-bucket/{prefix}/{path}`
///
/// 2. **Dedicated S3** (endpoint=Some, prefix=NULL):
///    User-provided S3/R2 endpoint with full bucket access.
///    → `https://user-s3.../user-bucket/{path}`
fn build_assets_url(config: &AssetsConfig, path: &str) -> Result<String, String> {
    // Default R2 endpoint if not specified
    let endpoint = config
        .endpoint
        .as_deref()
        .unwrap_or("https://r2.cloudflarestorage.com");

    // Clean up the path (remove leading slash)
    let path = path.trim_start_matches('/');

    // Build full path with prefix if specified
    let full_path = match &config.prefix {
        Some(prefix) => format!("{}/{}", prefix.trim_matches('/'), path),
        None => path.to_string(),
    };

    Ok(format!("{}/{}/{}", endpoint, config.bucket, full_path))
}

/// Sign an S3/R2 request using AWS Signature V4.
///
/// R2 uses AWS Signature V4 for authentication (not Bearer tokens).
/// The region is always "auto" for R2.
fn sign_s3_request(
    request: &HttpRequest,
    access_key_id: &str,
    secret_access_key: &str,
    _bucket: &str,
) -> Result<HashMap<String, String>, String> {
    // Parse the URL to get host and path
    let url = url::Url::parse(&request.url).map_err(|e| format!("Invalid URL: {}", e))?;

    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // Build HTTP method string
    let method = match request.method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Head => "HEAD",
        HttpMethod::Options => "OPTIONS",
    };

    // Create credentials
    let credentials = Credentials::new(
        access_key_id,
        secret_access_key,
        None, // session token
        None, // expiry
        "openworkers",
    );
    let identity = credentials.into();

    // Create signing settings
    let mut settings = SigningSettings::default();
    settings.signature_location = aws_sigv4::http_request::SignatureLocation::Headers;

    // Create signing params
    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region("auto") // R2 uses "auto" as region
        .name("s3") // Service name
        .time(SystemTime::now())
        .settings(settings)
        .build()
        .map_err(|e| format!("Failed to build signing params: {}", e))?;

    // Build the headers for signing
    let mut headers_to_sign = vec![("host", host.to_string())];

    // Add x-amz-content-sha256 header (required for S3)
    let body_hash = match &request.body {
        RequestBody::None => "UNSIGNED-PAYLOAD".to_string(),
        RequestBody::Bytes(b) => {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(b);
            hex::encode(hasher.finalize())
        }
    };
    headers_to_sign.push(("x-amz-content-sha256", body_hash.clone()));

    // Build signable request
    let signable_body = match &request.body {
        RequestBody::None => SignableBody::UnsignedPayload,
        RequestBody::Bytes(b) => SignableBody::Bytes(b),
    };

    let signable_request = SignableRequest::new(
        method,
        &request.url,
        headers_to_sign
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect::<Vec<_>>()
            .into_iter(),
        signable_body,
    )
    .map_err(|e| format!("Failed to create signable request: {}", e))?;

    // Sign the request
    let signing_output = sign(signable_request, &signing_params.into())
        .map_err(|e| format!("Failed to sign request: {}", e))?;

    // Collect signed headers
    let mut signed_headers = HashMap::new();
    signed_headers.insert("host".to_string(), host.to_string());
    signed_headers.insert("x-amz-content-sha256".to_string(), body_hash);

    for (name, value) in signing_output.output().headers() {
        signed_headers.insert(name.to_string(), value.to_string());
    }

    Ok(signed_headers)
}

/// Execute an HTTP request via reqwest with streaming response.
async fn do_fetch(
    request: HttpRequest,
    stats: &OperationsStats,
    extra_headers: Option<&HashMap<String, String>>,
) -> Result<HttpResponse, String> {
    // Build the HTTP client
    let client = reqwest::Client::new();

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
    let stats_clone = Arc::new(OperationsStats::new());

    // Copy stats for the spawned task
    let fetch_bytes_in = stats.fetch_bytes_in.load(Ordering::Relaxed);
    stats_clone
        .fetch_bytes_in
        .store(fetch_bytes_in, Ordering::Relaxed);

    tokio::spawn(async move {
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
