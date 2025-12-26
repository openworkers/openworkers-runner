//! Operations implementation for the runner
//!
//! This module implements the OperationsHandler trait from openworkers-core,
//! providing fetch with stats tracking.
//!
//! Note: Bindings (storage, database) will be separate operations
//! with their own authentication - fetch() is pass-through.

use bytes::Bytes;
use openworkers_core::{
    HttpMethod, HttpRequest, HttpResponse, LogEvent, LogLevel, OpFuture, OperationsHandler,
    RequestBody, ResponseBody,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

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

/// Runner's implementation of OperationsHandler
///
/// Implements fetch via reqwest and logs via channel.
/// Only overrides `handle_fetch` and `handle_log` - uses default for bindings.
pub struct RunnerOperations {
    /// Stats for this worker
    pub stats: Arc<OperationsStats>,
    /// User ID for logging/quotas
    pub user_id: Option<String>,
    /// Worker ID for logging/quotas
    pub worker_id: Option<String>,
    /// Log sender (optional - if None, uses default stderr)
    pub log_tx: Option<LogTx>,
}

impl RunnerOperations {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(OperationsStats::new()),
            user_id: None,
            worker_id: None,
            log_tx: None,
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
}

impl Default for RunnerOperations {
    fn default() -> Self {
        Self::new()
    }
}

impl OperationsHandler for RunnerOperations {
    fn handle_fetch(&self, request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move {
            // Track stats
            self.stats.fetch_count.fetch_add(1, Ordering::Relaxed);

            // Log the request
            log::debug!(
                "[ops] fetch {} {} (user={:?}, worker={:?})",
                request.method,
                request.url,
                self.user_id,
                self.worker_id
            );

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

            // Add headers from request (pass-through)
            for (key, value) in &request.headers {
                req_builder = req_builder.header(key, value);
            }

            // Add body if present
            if let RequestBody::Bytes(body) = &request.body {
                self.stats
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
            let stats = self.stats.clone();

            tokio::spawn(async move {
                use futures::StreamExt;
                let mut stream = response.bytes_stream();

                while let Some(chunk_result) = stream.next().await {
                    match chunk_result {
                        Ok(chunk) => {
                            stats
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
        })
    }

    // Note: handle_binding_fetch uses the default (returns error)
    // TODO: Implement binding support with config lookup and auth injection

    fn handle_log(&self, level: LogLevel, message: String) {
        self.stats.log_count.fetch_add(1, Ordering::Relaxed);

        // Send to log channel if available
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
