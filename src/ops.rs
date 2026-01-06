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
use once_cell::sync::Lazy;
#[cfg(feature = "database")]
use openworkers_core::{DatabaseOp, DatabaseResult};
use openworkers_core::{
    HttpMethod, HttpRequest, HttpResponse, KvOp, KvResult, LogEvent, LogLevel, OpFuture,
    OperationsHandler, RequestBody, ResponseBody, StorageOp, StorageResult,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

use crate::limiter::BindingLimiters;
use crate::store::{
    AssetsConfig, Binding, DatabaseConfig, KvConfig, StorageConfig, WorkerBindingConfig,
};

/// Worker domains for internal routing (e.g., "workers.rocks,workers.dev.localhost")
/// URLs matching `*.{domain}` will be routed internally instead of going through DNS.
static WORKER_DOMAINS: Lazy<Vec<String>> = Lazy::new(|| {
    std::env::var("WORKER_DOMAINS")
        .unwrap_or_else(|_| "workers.rocks".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
});

/// Generate a unique request ID (32 chars total: prefix_hex)
fn generate_request_id(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let hex_len = 31 - prefix.len();
    format!("{}_{:0width$x}", prefix, nanos, width = hex_len)
}

/// Global HTTP client for connection pooling and reuse
///
/// Creating a new client per request is expensive:
/// - New connection pool each time
/// - No TCP/TLS connection reuse
/// - DNS resolution on each request
///
/// This shared client enables:
/// - Connection pooling (reuse existing connections)
/// - Keep-alive connections
/// - DNS caching
///
/// Timeouts:
/// - connect_timeout: 2s (DNS + TCP handshake)
/// - timeout: 10s (total request)
static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .pool_max_idle_per_host(10)
        .pool_idle_timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10))
        .build()
        .expect("Failed to create HTTP client")
});

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
    pub database: HashMap<String, DatabaseConfig>,
    pub worker: HashMap<String, WorkerBindingConfig>,
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
                Binding::Database { key, config } => {
                    configs.database.insert(key, config);
                }
                Binding::Worker { key, config } => {
                    configs.worker.insert(key, config);
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
    /// Binding limiters (rate limiting for fetch, KV, database, storage)
    pub limiters: BindingLimiters,
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
            limiters: BindingLimiters::default(),
        }
    }

    /// Create with custom limiters based on RuntimeLimits
    pub fn with_limiters(mut self, limiters: BindingLimiters) -> Self {
        self.limiters = limiters;
        self
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

/// Check if a fetch URL matches internal worker patterns and route directly.
///
/// Patterns are configured via WORKER_DOMAINS env var (default: "workers.rocks,workers.dev.localhost")
/// URLs matching `*.{domain}` will be routed internally.
///
/// Returns a modified request with internal routing if matched.
fn try_internal_worker_route(request: &HttpRequest) -> Option<HttpRequest> {
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

    Some(HttpRequest {
        url: internal_url,
        method: request.method.clone(),
        headers,
        body: request.body.clone(),
    })
}

impl OperationsHandler for RunnerOperations {
    /// Handle direct fetch: `fetch("https://example.com")`
    ///
    /// This is a pass-through fetch with no auth modification.
    /// Used when worker code calls the global `fetch()` function.
    ///
    /// Special case: URLs matching `*.workers.rocks` or `*.workers.dev.localhost`
    /// are routed internally to avoid DNS lookup and external network hop.
    fn handle_fetch(&self, request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move {
            // Acquire fetch limiter permit (blocks if at concurrent limit, errors if total exceeded)
            let _guard = self
                .limiters
                .fetch
                .acquire()
                .await
                .map_err(|e| e.to_string())?;

            self.stats.fetch_count.fetch_add(1, Ordering::Relaxed);

            log::debug!(
                "[ops] fetch {} {} (user={:?}, worker={:?})",
                request.method,
                request.url,
                self.user_id,
                self.worker_id
            );

            // Check if this is an internal worker URL that should be routed directly
            if let Some(internal_request) = try_internal_worker_route(&request) {
                log::debug!(
                    "[ops] fetch shortcut: {} -> internal routing via x-worker-name",
                    request.url
                );
                return do_fetch(internal_request, &self.stats, None).await;
            }

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
                // Acquire fetch limiter permit
                let _guard = self
                    .limiters
                    .fetch
                    .acquire()
                    .await
                    .map_err(|e| e.to_string())?;

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
                // Acquire storage limiter permit (storage fetch uses storage limiter)
                let _guard = self
                    .limiters
                    .storage
                    .acquire()
                    .await
                    .map_err(|e| e.to_string())?;

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
                // Acquire storage limiter permit
                if let Err(e) = self.limiters.storage.acquire().await {
                    return StorageResult::Error(e.to_string());
                }

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
            // Acquire KV limiter permit
            if let Err(e) = self.limiters.kv.acquire().await {
                return KvResult::Error(e.to_string());
            }

            match op {
                KvOp::Get { key } => {
                    let result = sqlx::query_scalar::<_, serde_json::Value>(
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
                    // Check value size (100KB limit)
                    const MAX_VALUE_SIZE: usize = 100 * 1024;
                    let value_str = match serde_json::to_string(&value) {
                        Ok(s) => s,
                        Err(e) => {
                            return KvResult::Error(format!("Invalid JSON value: {}", e));
                        }
                    };

                    if value_str.len() > MAX_VALUE_SIZE {
                        return KvResult::Error(format!(
                            "Value too large: {} bytes (max {} bytes)",
                            value_str.len(),
                            MAX_VALUE_SIZE
                        ));
                    }

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

    /// Handle database operation: `env.DB.query("SELECT ...")`
    ///
    /// When the `postgate` feature is enabled, uses postgate to execute
    /// validated SQL queries. Otherwise, returns "not implemented".
    #[cfg(feature = "database")]
    fn handle_binding_database(
        &self,
        binding: &str,
        op: DatabaseOp,
    ) -> OpFuture<'_, DatabaseResult> {
        let binding_name = binding.to_string();

        // Check if binding exists in database configs
        let config = match self.bindings.database.get(binding) {
            Some(c) => c.clone(),
            None => {
                return Box::pin(async move {
                    DatabaseResult::Error(format!(
                        "Database binding '{}' not configured",
                        binding_name
                    ))
                });
            }
        };

        // Get shared pool for schema mode
        let shared_pool = self.db_pool.clone();

        Box::pin(async move {
            // Acquire database limiter permit
            if let Err(e) = self.limiters.database.acquire().await {
                return DatabaseResult::Error(e.to_string());
            }

            match op {
                DatabaseOp::Query { sql, params } => {
                    log::debug!(
                        "[ops] database {} ({:?}) query: {} (params: {:?})",
                        binding_name,
                        config.provider,
                        sql,
                        params
                    );

                    // Validate the SQL using postgate parser (allow all operations)
                    let allowed_ops = std::collections::HashSet::new();

                    let parsed = match postgate::parse_and_validate(&sql, &allowed_ops) {
                        Ok(p) => p,
                        Err(e) => {
                            return DatabaseResult::Error(format!("SQL validation failed: {}", e));
                        }
                    };

                    let mode = QueryMode::from_parsed(&parsed);

                    // Execute based on provider mode
                    let result = if let Some(ref conn_str) = config.connection_string {
                        // postgres provider: direct connection
                        execute_with_connection_string(conn_str, &sql, &params, mode).await
                    } else if let Some(ref schema_name) = config.schema_name {
                        // platform provider: multi-tenant on shared pool
                        match shared_pool {
                            Some(pool) => {
                                execute_with_schema(&pool, schema_name, &sql, &params, mode).await
                            }
                            None => Err("Shared database pool not configured".to_string()),
                        }
                    } else {
                        Err(
                            "Database config has neither connection_string nor schema_name"
                                .to_string(),
                        )
                    };

                    match result {
                        Ok(json) => DatabaseResult::Rows(json),
                        Err(e) => DatabaseResult::Error(e),
                    }
                }
            }
        })
    }

    /// Handle worker binding: `env.SERVICE.fetch(request)`
    ///
    /// Executes a worker-to-worker call by making an internal HTTP request
    /// to the runner with x-worker-id header to route to the target worker.
    fn handle_binding_worker(
        &self,
        binding: &str,
        request: HttpRequest,
    ) -> OpFuture<'_, Result<HttpResponse, String>> {
        let binding_name = binding.to_string();

        // Check if binding exists in worker configs
        let config = match self.bindings.worker.get(binding) {
            Some(c) => c.clone(),
            None => {
                return Box::pin(async move {
                    Err(format!("Worker binding '{}' not configured", binding_name))
                });
            }
        };

        Box::pin(async move {
            log::debug!(
                "[ops] worker binding {} -> {} ({})",
                binding_name,
                config.name,
                config.id
            );

            // Build the internal URL for the target worker
            // Runner listens on port 8080
            let path = if request.url.starts_with('/') {
                request.url.clone()
            } else {
                format!("/{}", request.url)
            };
            let internal_url = format!("http://127.0.0.1:8080{}", path);

            // Create the request with x-worker-id header to route to target
            let mut headers = request.headers.clone();
            headers.insert("x-worker-id".to_string(), config.id.clone());
            headers.insert("x-request-id".to_string(), generate_request_id("binding"));

            let internal_request = HttpRequest {
                url: internal_url,
                method: request.method,
                headers,
                body: request.body,
            };

            // Execute the request through the runner
            do_fetch(internal_request, &OperationsStats::new(), None).await
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

/// Execute query with direct connection string
#[cfg(feature = "database")]
async fn execute_with_connection_string(
    connection_string: &str,
    sql: &str,
    params: &[String],
    mode: QueryMode,
) -> Result<String, String> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(connection_string)
        .await
        .map_err(|e| format!("Database connection failed: {}", e))?;

    match mode {
        QueryMode::Mutation => execute_mutation(&pool, sql, params).await,
        _ => execute_json_query(&pool, sql, params, mode).await,
    }
}

/// Execute query with schema isolation (SET search_path)
#[cfg(feature = "database")]
async fn execute_with_schema(
    pool: &DbPool,
    schema_name: &str,
    sql: &str,
    params: &[String],
    mode: QueryMode,
) -> Result<String, String> {
    // Use a transaction to set search_path, then execute the query
    let safe_schema = schema_name.replace('"', "\"\"");

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("Failed to start transaction: {}", e))?;

    // Set the search_path for this transaction
    sqlx::query(&format!("SET LOCAL search_path TO \"{}\"", safe_schema))
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("Failed to set search_path: {}", e))?;

    // Execute the user query
    let result = match mode {
        QueryMode::Mutation => execute_mutation_tx(&mut tx, sql, params).await?,
        _ => execute_json_query_tx(&mut tx, sql, params, mode).await?,
    };

    // Commit the transaction
    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit transaction: {}", e))?;

    Ok(result)
}

/// Query execution mode
#[cfg(feature = "database")]
#[derive(Clone, Copy)]
enum QueryMode {
    /// SELECT query - wrap as subquery
    Select,
    /// INSERT/UPDATE/DELETE with RETURNING - wrap as CTE
    ReturningMutation,
    /// INSERT/UPDATE/DELETE without RETURNING - return rows affected
    Mutation,
}

#[cfg(feature = "database")]
impl QueryMode {
    fn from_parsed(parsed: &postgate::ParsedQuery) -> Self {
        if !parsed.returns_rows {
            QueryMode::Mutation
        } else if parsed.operation == postgate::SqlOperation::Select {
            QueryMode::Select
        } else {
            QueryMode::ReturningMutation
        }
    }
}

/// Wrap user query to return JSON directly from PostgreSQL
#[cfg(feature = "database")]
fn wrap_query_as_json(sql: &str, mode: QueryMode) -> String {
    let trimmed = sql.trim().trim_end_matches(';');

    match mode {
        QueryMode::Select => {
            // SELECT can be used as a subquery
            format!(
                "SELECT COALESCE(jsonb_agg(row_to_json(t)), '[]'::jsonb) as result FROM ({}) t",
                trimmed
            )
        }
        QueryMode::ReturningMutation => {
            // INSERT/UPDATE/DELETE with RETURNING needs a CTE
            format!(
                "WITH t AS ({}) SELECT COALESCE(jsonb_agg(row_to_json(t)), '[]'::jsonb) as result FROM t",
                trimmed
            )
        }
        QueryMode::Mutation => {
            unreachable!("Mutation queries should not be wrapped")
        }
    }
}

/// Execute wrapped JSON query and extract result
#[cfg(feature = "database")]
async fn execute_json_query(
    pool: &DbPool,
    sql: &str,
    params: &[String],
    mode: QueryMode,
) -> Result<String, String> {
    use sqlx::Row;

    let wrapped = wrap_query_as_json(sql, mode);
    let mut query = sqlx::query(&wrapped);

    for param in params {
        query = query.bind(param);
    }

    let row = query
        .fetch_one(pool)
        .await
        .map_err(|e| format!("Query execution failed: {}", e))?;

    let result: serde_json::Value = row
        .try_get("result")
        .map_err(|e| format!("Failed to get result: {}", e))?;

    serde_json::to_string(&result).map_err(|e| format!("Failed to serialize result: {}", e))
}

/// Execute wrapped JSON query within a transaction
#[cfg(feature = "database")]
async fn execute_json_query_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sql: &str,
    params: &[String],
    mode: QueryMode,
) -> Result<String, String> {
    use sqlx::Row;

    let wrapped = wrap_query_as_json(sql, mode);
    let mut query = sqlx::query(&wrapped);

    for param in params {
        query = query.bind(param);
    }

    let row = query
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| format!("Query execution failed: {}", e))?;

    let result: serde_json::Value = row
        .try_get("result")
        .map_err(|e| format!("Failed to get result: {}", e))?;

    serde_json::to_string(&result).map_err(|e| format!("Failed to serialize result: {}", e))
}

/// Execute mutation (INSERT/UPDATE/DELETE) and return rows affected
#[cfg(feature = "database")]
async fn execute_mutation(pool: &DbPool, sql: &str, params: &[String]) -> Result<String, String> {
    let mut query = sqlx::query(sql);

    for param in params {
        query = query.bind(param);
    }

    let result = query
        .execute(pool)
        .await
        .map_err(|e| format!("Query execution failed: {}", e))?;

    Ok(format!("{{\"rowsAffected\":{}}}", result.rows_affected()))
}

/// Execute mutation within a transaction
#[cfg(feature = "database")]
async fn execute_mutation_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sql: &str,
    params: &[String],
) -> Result<String, String> {
    let mut query = sqlx::query(sql);

    for param in params {
        query = query.bind(param);
    }

    let result = query
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("Query execution failed: {}", e))?;

    Ok(format!("{{\"rowsAffected\":{}}}", result.rows_affected()))
}

/// Sanitize a path to prevent directory traversal attacks.
///
/// Removes `.` and `..` components, preventing escape from prefix boundaries.
/// Returns None if the path would escape the root (e.g., `../secret`).
fn sanitize_path(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();

    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                // Trying to go above root - reject
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }

    Some(parts.join("/"))
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

    // Build full path with prefix if specified
    let full_path = match &config.prefix {
        Some(prefix) => {
            // Sanitize path to prevent directory traversal (../) only when there's a prefix
            // This prevents escaping the prefix in multi-tenant shared buckets
            let sanitized_path = sanitize_path(path)
                .ok_or_else(|| "Invalid path: traversal not allowed".to_string())?;
            format!("{}/{}", prefix.trim_matches('/'), sanitized_path)
        }
        None => {
            // BYOS (dedicated bucket) - no prefix, user has full bucket access
            // No need to sanitize, they can access any path in their own bucket
            path.trim_start_matches('/').to_string()
        }
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
    // Use global HTTP client for connection pooling
    let client = &*HTTP_CLIENT;

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

    // Use spawn_local to keep the streaming task on the same thread as the V8 isolate.
    // This works because the entire call chain (worker -> event_loop -> ops) uses spawn_local,
    // keeping everything within the same LocalSet.
    tokio::task::spawn_local(async move {
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
