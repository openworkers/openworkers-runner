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

#[cfg(feature = "database")]
use openworkers_core::{DatabaseOp, DatabaseResult};
use openworkers_core::{
    HttpMethod, HttpRequest, HttpResponse, KvOp, KvResult, LogEvent, LogLevel, OpFuture,
    OperationsHandler, RequestBody, StorageOp, StorageResult,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::limiter::BindingLimiters;
#[cfg(feature = "database")]
use crate::services::database::{self as db_service, QueryMode};
use crate::services::fetch::{do_fetch, generate_request_id, try_internal_worker_route};
use crate::services::kv as kv_service;
use crate::services::storage::{build_s3_url, execute_s3_operation, sign_s3_request};
use crate::store::{Binding, DatabaseConfig, KvConfig, StorageConfig, WorkerBindingConfig};

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
    pub assets: HashMap<String, StorageConfig>,
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

    /// Execute a binding fetch with the given config (shared by assets and storage).
    fn do_binding_fetch(
        &self,
        binding: &str,
        config: StorageConfig,
        request: HttpRequest,
    ) -> OpFuture<'_, Result<HttpResponse, String>> {
        let binding_name = binding.to_string();

        Box::pin(async move {
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
            let url = build_s3_url(&config, &request.url)?;

            // Create modified request with AWS signature
            // Filter out Host header to avoid 421 Misdirected Request from S3/R2
            let mut headers = request.headers.clone();
            headers.remove("host");
            headers.remove("Host");

            let auth_request = HttpRequest {
                url: url.clone(),
                method: request.method,
                headers,
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
        })
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

    /// Handle binding fetch: `env.ASSETS.fetch("/path")` or `env.STORAGE.fetch("/path")`
    ///
    /// This is called when worker code uses a binding's fetch method.
    /// The runner:
    /// 1. Looks up the binding config by name (assets or storage)
    /// 2. Builds the actual S3/R2 URL (endpoint + bucket + prefix + path)
    /// 3. Injects auth headers (AWS sig for S3)
    /// 4. Executes the HTTP request with streaming response
    ///
    /// The worker never sees the credentials - only the binding name.
    fn handle_binding_fetch(
        &self,
        binding: &str,
        request: HttpRequest,
    ) -> OpFuture<'_, Result<HttpResponse, String>> {
        // Assets bindings
        if let Some(config) = self.bindings.assets.get(binding) {
            return self.do_binding_fetch(binding, config.clone(), request);
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

    /// Handle storage operation: `env.STORAGE.get("key")`, `.put()`, `.head()`, `.list()`, `.delete()`, `.fetch()`
    ///
    /// Uses AWS S3 v4 signing to authenticate requests to S3-compatible storage.
    /// Note: `StorageOp::Fetch` uses streaming via `do_fetch` for efficiency.
    fn handle_binding_storage(&self, binding: &str, op: StorageOp) -> OpFuture<'_, StorageResult> {
        let binding_name = binding.to_string();

        // Check if binding exists in storage configs
        let config = match self.bindings.storage.get(binding) {
            Some(c) => c.clone(),
            None => {
                return Box::pin(async move {
                    StorageResult::Error(format!(
                        "Storage binding '{}' not configured",
                        binding_name
                    ))
                });
            }
        };

        Box::pin(async move {
            // Acquire storage limiter permit
            if let Err(e) = self.limiters.storage.acquire().await {
                return StorageResult::Error(e.to_string());
            }

            match op {
                StorageOp::Fetch { key } => {
                    log::debug!("[ops] storage {} fetch({})", binding_name, key);

                    let request = HttpRequest {
                        url: key,
                        method: HttpMethod::Get,
                        headers: Default::default(),
                        body: RequestBody::None,
                    };

                    match self.do_binding_fetch(&binding_name, config, request).await {
                        Ok(response) => StorageResult::Response(response),
                        Err(e) => StorageResult::Error(e),
                    }
                }

                other => {
                    let op_name = match &other {
                        StorageOp::Get { key } => format!("get({})", key),
                        StorageOp::Fetch { .. } => unreachable!(),
                        StorageOp::Put { key, .. } => format!("put({})", key),
                        StorageOp::Head { key } => format!("head({})", key),
                        StorageOp::List { prefix, .. } => format!("list(prefix={:?})", prefix),
                        StorageOp::Delete { key } => format!("delete({})", key),
                    };

                    log::debug!("[ops] storage {} {}", binding_name, op_name);

                    execute_s3_operation(&config, other).await
                }
            }
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
                KvOp::Get { key } => kv_service::get(&pool, &namespace_id, &key).await,
                KvOp::Put {
                    key,
                    value,
                    expires_in,
                } => kv_service::put(&pool, &namespace_id, &key, &value, expires_in).await,
                KvOp::Delete { key } => kv_service::delete(&pool, &namespace_id, &key).await,
                KvOp::List { prefix, limit } => {
                    kv_service::list(&pool, &namespace_id, prefix.as_deref(), limit).await
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
            // Acquire database limiter permit - hold guard until query completes
            let _guard = match self.limiters.database.acquire().await {
                Ok(guard) => guard,
                Err(e) => return DatabaseResult::Error(e.to_string()),
            };

            log::debug!(
                "[ops] database limiter acquired, available permits: {}",
                self.limiters.database.available_permits()
            );

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
                        db_service::execute_with_connection_string(conn_str, &sql, &params, mode)
                            .await
                    } else if let Some(ref schema_name) = config.schema_name {
                        // platform provider: multi-tenant on shared pool
                        match shared_pool {
                            Some(pool) => {
                                db_service::execute_with_schema(
                                    &pool,
                                    schema_name,
                                    &sql,
                                    &params,
                                    mode,
                                )
                                .await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runner_operations_builder() {
        let ops = RunnerOperations::new();
        assert!(ops.bindings.assets.is_empty());
        assert!(ops.bindings.storage.is_empty());
        assert!(ops.bindings.kv.is_empty());
    }

    #[test]
    fn test_runner_operations_with_user() {
        let ops = RunnerOperations::new().with_user_id("user-123".to_string());
        assert_eq!(ops.user_id, Some("user-123".to_string()));
    }

    #[test]
    fn test_runner_operations_with_worker() {
        let ops = RunnerOperations::new().with_worker_id("worker-456".to_string());
        assert_eq!(ops.worker_id, Some("worker-456".to_string()));
    }

    #[test]
    fn test_runner_operations_stats_initial() {
        let ops = RunnerOperations::new();
        assert_eq!(ops.stats.fetch_count.load(Ordering::Relaxed), 0);
        assert_eq!(ops.stats.fetch_bytes_in.load(Ordering::Relaxed), 0);
        assert_eq!(ops.stats.fetch_bytes_out.load(Ordering::Relaxed), 0);
    }
}
