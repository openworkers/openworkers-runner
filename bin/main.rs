use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpSocket;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot::channel;
use tracing::{debug, error, info, warn};

use openworkers_core::{HttpRequest, HttpResponse, HyperBody};
use openworkers_runner::store::WorkerIdentifier;

use sqlx::postgres::PgPoolOptions;

struct AppState {
    db: sqlx::Pool<sqlx::Postgres>,
    log_tx: std::sync::mpsc::Sender<openworkers_runner::log::LogMessage>,
    shutdown_tx: tokio::sync::mpsc::Sender<()>,
    wall_clock_timeout_ms: u64,
}

async fn handle_request(
    state: &AppState,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<HyperBody>, std::convert::Infallible> {
    let is_local = req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .map(|h| h == "127.0.0.1:8080" || h.starts_with("localhost"))
        .unwrap_or(false);

    // Admin endpoints (only for local requests)
    if is_local {
        let path = req.uri().path();

        // Health check endpoint
        if path == "/health" {
            return Ok(Response::builder()
                .status(200)
                .body(full_body("OK"))
                .unwrap());
        }

        // POST /admin/drain - Put runner in draining mode
        if path == "/admin/drain" && req.method() == hyper::Method::POST {
            openworkers_runner::worker_pool::set_draining(true);

            // Check if already no active tasks - shutdown immediately
            if openworkers_runner::worker_pool::get_active_tasks() == 0 {
                debug!("Drain called with 0 active tasks - shutting down immediately");
                let _ = state.shutdown_tx.send(()).await;
            }

            return Ok(Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(full_body(r#"{"draining":true}"#))
                .unwrap());
        }

        // GET /admin/tasks - Get number of active tasks
        if path == "/admin/tasks" && req.method() == hyper::Method::GET {
            let active = openworkers_runner::worker_pool::get_active_tasks();
            let body = format!(r#"{{"active_tasks":{}}}"#, active);
            return Ok(Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(full_body(body))
                .unwrap());
        }

        // GET /admin/stats - Get runner stats
        if path == "/admin/stats" && req.method() == hyper::Method::GET {
            let active = openworkers_runner::worker_pool::get_active_tasks();
            let draining = openworkers_runner::worker_pool::is_draining();
            let body = format!(r#"{{"active_tasks":{},"draining":{}}}"#, active, draining);
            return Ok(Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(full_body(body))
                .unwrap());
        }

        // GET /admin/pool - Get isolate pool statistics
        #[cfg(feature = "v8")]
        if path == "/admin/pool" && req.method() == hyper::Method::GET {
            let stats = openworkers_runtime_v8::get_pinned_pool_stats();

            let body = format!(
                r#"{{"total_requests":{},"cache_hits":{},"cache_misses":{},"hit_rate":{:.2}}}"#,
                stats.total_requests, stats.cache_hits, stats.cache_misses, stats.hit_rate
            );

            return Ok(Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(full_body(body))
                .unwrap());
        }
    }

    // Check if runner is draining - refuse new worker requests
    if openworkers_runner::worker_pool::is_draining() {
        warn!("Refusing request while draining");
        return Ok(Response::builder()
            .status(503)
            .header("content-type", "text/plain")
            .body(full_body(
                "503 - Runner is draining - not accepting new requests",
            ))
            .unwrap());
    }

    // Extract parts before consuming the body
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    // Log request before span creation (worker not yet identified)
    debug!(
        "handle_request: {} {} in thread {:?}, x-worker-id: {:?}, x-worker-name: {:?}",
        method,
        uri,
        std::thread::current().id(),
        headers.get("x-worker-id").and_then(|h| h.to_str().ok()),
        headers.get("x-worker-name").and_then(|h| h.to_str().ok())
    );

    // Acquire database connection
    let mut conn: sqlx::pool::PoolConnection<sqlx::Postgres> = match state.db.acquire().await {
        Ok(db) => db,
        Err(err) => {
            error!("Failed to acquire database connection: {}", err);
            return Ok(error_response(500, "Failed to acquire database connection"));
        }
    };

    // Extract x-request-id header
    let request_id = match headers.get("x-request-id") {
        Some(value) => match value.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return Ok(error_response(400, "Invalid x-request-id header encoding")),
        },
        None => return Ok(error_response(400, "Missing request id")),
    };

    // Create tracing span for this request
    let span = tracing::info_span!(
        "request",
        request_id = %request_id,
        http_method = %method,
        uri = %uri,
        http_host = tracing::field::Empty,
        response_status_code = tracing::field::Empty,
        backend_type = tracing::field::Empty,
        worker_id = tracing::field::Empty,
        worker_name = tracing::field::Empty,
        user_id = tracing::field::Empty,
    );

    // Use Instrument trait for async operations
    use tracing::Instrument;

    // Execute the rest of the handler within the span context
    handle_worker_request(state, span.clone(), &mut conn, req)
        .instrument(span)
        .await
}

/// Inner handler that executes within the request span context
async fn handle_worker_request(
    state: &AppState,
    span: tracing::Span,
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<HyperBody>, std::convert::Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    // Start metrics timer
    let mut metrics_timer = openworkers_runner::metrics::MetricsTimer::new();

    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    // Record HTTP host header (even if worker not found)
    if let Some(ref hostname) = host {
        span.record("http_host", hostname.as_str());
    }

    let worker_id = headers
        .get("x-worker-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let worker_name = headers
        .get("x-worker-name")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    debug!(
        "handle_request: {} {}{} ({})",
        method,
        host.as_deref().unwrap_or(""),
        uri,
        worker_id
            .as_deref()
            .or(worker_name.as_deref())
            .unwrap_or("no worker info")
    );

    let path = req.uri().path();

    debug!(
        "host: {:?}, worker_id: {:?}, worker_name: {:?}, path: {}",
        host, worker_id, worker_name, path
    );

    // Resolve request in a single DB call (handles endpoint resolution + routing)
    let resolution = openworkers_runner::store::resolve_worker_from_request(
        conn,
        host.as_deref(),
        worker_id.as_deref(),
        worker_name.as_deref(),
        path,
    )
    .await;

    debug!("Request resolved: {:?}", resolution);

    // Handle resolution result
    let worker = match resolution {
        Some(res) => {
            use openworkers_runner::BackendType;
            use openworkers_runner::store::get_worker_with_bindings;

            // Check if backend_type is None
            let backend_type = match res.backend_type {
                Some(bt) => bt,
                None => {
                    // If neither worker_id nor project_id is set, worker/project not found (502)
                    // If at least one is set, route not found (404)
                    if res.worker_id.is_none() && res.project_id.is_none() {
                        return Ok(error_response_with_span(
                            &span,
                            502,
                            "No worker or project found for request",
                        ));
                    } else {
                        return Ok(error_response_with_span(&span, 404, "Not Found"));
                    }
                }
            };

            // Project routing: check backend type
            if res.project_id.is_some() {
                // Record backend type for telemetry
                span.record("backend_type", tracing::field::display(&backend_type));

                match backend_type {
                    BackendType::Worker => {
                        if let Some(worker_id) = res.worker_id {
                            let worker =
                                get_worker_with_bindings(conn, WorkerIdentifier::Id(worker_id))
                                    .await;

                            match worker {
                                Some(w) => w,
                                None => {
                                    return Ok(error_response_with_span(
                                        &span,
                                        500,
                                        "Route has worker backend but worker not found",
                                    ));
                                }
                            }
                        } else {
                            return Ok(error_response_with_span(
                                &span,
                                500,
                                "Route has worker backend but no worker_id",
                            ));
                        }
                    }
                    BackendType::Storage => {
                        use openworkers_core::{
                            HttpMethod, HttpRequest, OperationsHandler, RequestBody,
                        };

                        // Serve static file from storage via ASSETS binding
                        let storage_config_id = match &res.assets_storage_id {
                            Some(id) => id,
                            None => {
                                return Ok(error_response_with_span(
                                    &span,
                                    500,
                                    "ASSETS storage config not found",
                                ));
                            }
                        };

                        // Load storage config
                        let storage_config =
                            openworkers_runner::store::get_storage_config(conn, storage_config_id)
                                .await;

                        let storage_config = match storage_config {
                            Some(config) => config,
                            None => {
                                return Ok(error_response_with_span(
                                    &span,
                                    500,
                                    "Storage config not found",
                                ));
                            }
                        };

                        // Create RunnerOperations with ASSETS binding to get limiters and stats
                        let ops = openworkers_runner::RunnerOperations::new().with_bindings(vec![
                            openworkers_runner::store::Binding::Assets {
                                key: "ASSETS".to_string(),
                                config: storage_config,
                            },
                        ]);

                        // Transform path for prerendered routes (add .html extension)
                        // Static routes are stored with .html extension and don't need transformation
                        let request_path = if matches!(
                            res.asset_type,
                            Some(openworkers_runner::store::AssetType::Prerendered)
                        ) && !path.contains('.')
                        {
                            if path == "/" || path.is_empty() {
                                "/index.html".to_string()
                            } else {
                                format!("{}.html", path)
                            }
                        } else {
                            path.to_string()
                        };

                        // Create HttpRequest for binding fetch
                        let http_request = HttpRequest {
                            url: request_path,
                            method: HttpMethod::Get,
                            headers: std::collections::HashMap::new(),
                            body: RequestBody::None,
                        };

                        // Use handle_binding_fetch to get limiters and stats
                        match ops.handle_binding_fetch("ASSETS", http_request).await {
                            Ok(http_response) => {
                                debug!(
                                    "handle_request done (storage) in {}ms",
                                    metrics_timer.start.elapsed().as_millis()
                                );

                                // Record status code and convert to hyper response
                                let hyper_response = http_response.into_hyper();
                                span.record(
                                    "response_status_code",
                                    hyper_response.status().as_u16(),
                                );
                                return Ok(hyper_response);
                            }
                            Err(e) => {
                                debug!(
                                    "handle_request done (storage) in {}ms (failed)",
                                    metrics_timer.start.elapsed().as_millis()
                                );

                                error!("Failed to fetch from storage: {}", e);
                                span.record("response_status_code", 500u16);
                                return Ok(error_response_with_span(
                                    &span,
                                    500,
                                    "Failed to fetch from storage",
                                ));
                            }
                        }
                    }
                }
            }
            // Standalone worker
            else if let Some(worker_id) = res.worker_id {
                // Record backend type for standalone workers
                span.record("backend_type", tracing::field::display(BackendType::Worker));
                let worker = get_worker_with_bindings(conn, WorkerIdentifier::Id(worker_id)).await;

                match worker {
                    Some(w) => w,
                    None => {
                        return Ok(error_response_with_span(
                            &span,
                            500,
                            "Route has worker_id but worker not found",
                        ));
                    }
                }
            } else {
                return Ok(error_response_with_span(
                    &span,
                    500,
                    "Invalid resolution: no worker_id or project_id",
                ));
            }
        }
        None => {
            return Ok(error_response_with_span(
                &span,
                502,
                "No worker or project found for request",
            ));
        }
    };

    // Record worker info in span now that we have it
    span.record("worker_id", tracing::field::display(&worker.id));
    if let Some(name) = &worker.name {
        span.record("worker_name", tracing::field::display(name));
    }
    span.record("user_id", tracing::field::display(&worker.user_id));

    // Add metrics labels
    #[cfg(feature = "telemetry")]
    {
        use opentelemetry::KeyValue;
        metrics_timer = metrics_timer.with_labels(vec![
            KeyValue::new("method", method.to_string()),
            KeyValue::new("worker_id", worker.id.clone()),
            KeyValue::new("user_id", worker.user_id.clone()),
        ]);
    }

    let start = tokio::time::Instant::now();

    // Collect request body (consumes req)
    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return Ok(error_response(400, "Failed to read request body"));
        }
    };

    // Convert to our HttpRequest using the extracted parts
    let mut request = HttpRequest::from_hyper_parts(&method, &uri, &headers, body_bytes, "http");

    // Add worker headers if not present
    if !request.headers.contains_key("x-worker-id") {
        request
            .headers
            .insert("x-worker-id".to_string(), worker.id.clone());
    }

    if !request.headers.contains_key("x-worker-name") {
        if let Some(name) = &worker.name {
            request
                .headers
                .insert("x-worker-name".to_string(), name.clone());
        }
    }

    // Acquire worker slot with timeout
    let timeout = openworkers_runner::worker_pool::get_worker_wait_timeout();
    let permit = match tokio::time::timeout(
        timeout,
        openworkers_runner::worker_pool::WORKER_SEMAPHORE
            .clone()
            .acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => {
            error!("semaphore closed unexpectedly");
            return Ok(error_response(500, "Internal server error"));
        }
        Err(_) => {
            debug!(
                "worker pool saturated after {}ms timeout, returning 503",
                timeout.as_millis()
            );
            return Ok(error_response(
                503,
                "Server is overloaded, please try again later",
            ));
        }
    };

    let (res_tx, res_rx) = channel::<HttpResponse>();
    let (termination_tx, termination_rx) =
        channel::<Result<(), openworkers_core::TerminationReason>>();

    // Create disconnect notification channel
    let (disconnect_tx, _disconnect_rx) = channel::<()>();

    // Mark worker spawned (for queue time metric)
    metrics_timer.mark_worker_spawned();

    openworkers_runner::event_fetch::run_fetch(
        worker,
        request,
        res_tx,
        termination_tx,
        state.log_tx.clone(),
        permit,
        state.db.clone(),
        state.wall_clock_timeout_ms,
        span.clone(),
    );

    // TODO: Pass disconnect_rx to the worker so it can stop processing

    let (response, success) = match res_rx.await {
        Ok(res) => {
            // Convert to hyper Response with disconnect notification
            (res.into_hyper_with_disconnect(Some(disconnect_tx)), true)
        }
        Err(_) => {
            // Worker didn't send response, check termination reason
            use openworkers_core::TerminationReason;

            let result = termination_rx
                .await
                .unwrap_or(Err(TerminationReason::Other("Unknown error".to_string())));

            let resp = match result {
                Ok(()) => {
                    error!("worker completed but did not send response");
                    error_response(
                        500,
                        "Worker completed but did not send a response (missing fetch event listener?)",
                    )
                }
                Err(reason) => {
                    error!("worker terminated without sending response: {:?}", reason);
                    let mut resp = error_response(reason.http_status(), &reason.to_string());
                    // Add termination reason header
                    *resp.headers_mut() = {
                        let mut headers = resp.headers().clone();
                        headers.insert(
                            "x-termination-reason",
                            format!("{:?}", reason).parse().unwrap(),
                        );
                        headers
                    };
                    resp
                }
            };
            (resp, false)
        }
    };

    debug!("handle_request done in {}ms", start.elapsed().as_millis());

    // Record response status code and metrics
    span.record("response_status_code", response.status().as_u16());
    metrics_timer.record_http_request(success);

    Ok(response)
}

fn full_body(content: impl Into<Bytes>) -> HyperBody {
    HyperBody::Full(http_body_util::Full::new(content.into()))
}

fn error_response(status: u16, message: &str) -> Response<HyperBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(full_body(message.to_string()))
        .unwrap()
}

fn error_response_with_span(
    span: &tracing::Span,
    status: u16,
    message: &str,
) -> Response<HyperBody> {
    span.record("response_status_code", status);
    error_response(status, message)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    openworkers_runner::telemetry::init();

    debug!("start main (hyper)");

    // Parse isolate pool configuration from environment
    let pool_max_size = std::env::var("ISOLATE_POOL_SIZE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1000); // Default: 1000 isolates

    let heap_initial_mb = std::env::var("ISOLATE_HEAP_INITIAL_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10); // Default: 10MB

    let heap_max_mb = std::env::var("ISOLATE_HEAP_MAX_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(50); // Default: 50MB

    let wall_clock_timeout_ms = std::env::var("WALL_CLOCK_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(64_000); // Default: 64 seconds

    let cpu_time_limit_ms = std::env::var("CPU_TIME_LIMIT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(100); // Default: 100ms

    let v8_execute_mode = openworkers_runner::V8ExecuteMode::get();

    debug!(
        "Isolate pool config: max_size={}, heap_initial={}MB, heap_max={}MB, wall_clock_timeout={}ms, cpu_time_limit={}ms",
        pool_max_size, heap_initial_mb, heap_max_mb, wall_clock_timeout_ms, cpu_time_limit_ms
    );
    debug!("V8 execution mode: {:?}", v8_execute_mode);

    let db_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            let host = std::env::var("POSTGRES_HOST").expect("POSTGRES_HOST must be set");
            let port = std::env::var("POSTGRES_PORT").expect("POSTGRES_PORT must be set");
            let user = std::env::var("POSTGRES_USER").expect("POSTGRES_USER must be set");
            let password =
                std::env::var("POSTGRES_PASSWORD").expect("POSTGRES_PASSWORD must be set");
            let database = std::env::var("POSTGRES_DB").expect("POSTGRES_DB must be set");

            debug!("DATABASE_URL not set, using POSTGRES_* env vars");

            format!("postgres://{user}:{password}@{host}:{port}/{database}")
        }
    };

    // Skip connection health check before acquire (faster, use when DB is local/stable)
    let test_before_acquire = std::env::var("DB_TEST_BEFORE_ACQUIRE")
        .map(|v| !matches!(v.as_str(), "false" | "0"))
        .unwrap_or(true);

    // Retry database connection with exponential backoff
    let mut retry_count = 0;
    let max_retries = 5;
    let pool = loop {
        match PgPoolOptions::new()
            .max_connections(20)
            .acquire_timeout(Duration::from_secs(5))
            .test_before_acquire(test_before_acquire)
            .connect(&db_url)
            .await
        {
            Ok(pool) => match sqlx::query("SELECT 1").fetch_one(&pool).await {
                Ok(_) => {
                    debug!("connected to Postgres");
                    break pool;
                }
                Err(e) => {
                    error!("Database connection test failed: {}", e);
                    if retry_count >= max_retries {
                        panic!(
                            "Failed to connect to database after {} retries",
                            max_retries
                        );
                    }
                }
            },
            Err(e) => {
                retry_count += 1;
                if retry_count > max_retries {
                    panic!(
                        "Failed to connect to database after {} retries: {}",
                        max_retries, e
                    );
                }
                let wait_time = Duration::from_secs(2u64.pow(retry_count.min(5)));
                warn!(
                    "Database connection attempt {} failed: {}. Retrying in {:?}...",
                    retry_count, e, wait_time
                );
                tokio::time::sleep(wait_time).await;
            }
        }
    };

    // Connect to NATS with retries
    let mut retry_count = 0;
    loop {
        match openworkers_runner::nats::nats_connect()
            .await
            .publish("boot", "0".into())
            .await
        {
            Ok(_) => {
                debug!("connected to NATS");
                break;
            }
            Err(e) => {
                retry_count += 1;
                if retry_count > max_retries {
                    panic!(
                        "Failed to connect to NATS after {} retries: {}",
                        max_retries, e
                    );
                }
                let wait_time = Duration::from_secs(2u64.pow(retry_count.min(5)));
                warn!(
                    "NATS connection attempt {} failed: {}. Retrying in {:?}...",
                    retry_count, e, wait_time
                );
                tokio::time::sleep(wait_time).await;
            }
        }
    }

    // Start global log publisher
    let log_tx = openworkers_runner::log::start_log_publisher();
    debug!("started log publisher");

    // Initialize isolate pool for V8 runtime
    #[cfg(feature = "v8")]
    {
        use openworkers_core::RuntimeLimits;

        let pool_limits = RuntimeLimits {
            heap_initial_mb,
            heap_max_mb,
            max_cpu_time_ms: cpu_time_limit_ms,
            max_wall_clock_time_ms: wall_clock_timeout_ms,
            ..Default::default()
        };

        match v8_execute_mode {
            openworkers_runner::V8ExecuteMode::Pinned => {
                openworkers_runtime_v8::init_pinned_pool(pool_max_size, pool_limits);
                debug!(
                    "Initialized pinned isolate pool with {} isolates max",
                    pool_max_size
                );
            }
            openworkers_runner::V8ExecuteMode::Pooled => {
                openworkers_runtime_v8::init_pool(pool_max_size, pool_limits);
                debug!(
                    "Initialized global isolate pool with {} isolates max",
                    pool_max_size
                );
            }
            openworkers_runner::V8ExecuteMode::Oneshot => {
                debug!("Oneshot mode: no isolate pool initialized");
            }
        }
    }

    openworkers_runner::event_scheduled::handle_scheduled(pool.clone(), log_tx.clone());

    // Shutdown signal channel
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let shutdown_tx_signal = shutdown_tx.clone();
    let shutdown_tx_drain = shutdown_tx.clone();

    let state = std::sync::Arc::new(AppState {
        db: pool,
        log_tx,
        shutdown_tx,
        wall_clock_timeout_ms,
    });

    // Signal handler for SIGINT/SIGTERM - graceful on first, forced on second
    tokio::spawn(async move {
        let mut sigterm = signal(SignalKind::terminate()).expect("Failed to setup SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("Failed to setup SIGINT handler");

        // First signal
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv() => {},
        }

        tracing::info!(
            "Received signal - initiating graceful drain (press Ctrl+C again to force exit)"
        );
        openworkers_runner::worker_pool::set_draining(true);

        // Check if already no active tasks
        if openworkers_runner::worker_pool::get_active_tasks() == 0 {
            tracing::info!("No active tasks - shutting down immediately");
            let _ = shutdown_tx_signal.send(()).await;
            return;
        }

        // Second signal
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv() => {},
        }

        tracing::warn!("Received second signal - forcing exit");
        std::process::exit(1);
    });

    // Start drain monitor
    tokio::spawn(async move {
        let notify = openworkers_runner::worker_pool::TASK_COMPLETION_NOTIFY.clone();

        loop {
            // Wait for task completion notification
            notify.notified().await;

            if openworkers_runner::worker_pool::is_draining() {
                let active = openworkers_runner::worker_pool::get_active_tasks();
                debug!("Draining: {} active tasks remaining", active);

                if active == 0 {
                    tracing::info!("All tasks completed while draining - shutting down gracefully");
                    let _ = shutdown_tx_drain.send(()).await;
                    break;
                }
            }
        }
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let num_listeners = std::env::var("HTTP_LISTENERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        });

    info!(
        "Listening on http://{} with {} listeners",
        addr, num_listeners
    );

    for i in 0..num_listeners {
        let socket = TcpSocket::new_v4()?;
        socket.set_reuseport(true)?;
        socket.bind(addr)?;

        let listener = socket.listen(1024)?;
        let state = state.clone();

        tokio::spawn(async move {
            debug!("Listener {} started", i);

            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        error!("Accept error on listener {}: {:?}", i, e);
                        continue;
                    }
                };

                let io = TokioIo::new(stream);
                let state = state.clone();

                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let state = state.clone();
                        async move { handle_request(&state, req).await }
                    });

                    if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                        // Connection errors are normal (client disconnect, etc.)
                        debug!("Connection error: {:?}", err);
                    }
                });
            }
        });
    }

    // Wait for graceful shutdown signal
    shutdown_rx.recv().await;

    tracing::info!("Shutdown signal received - exiting");

    Ok(())
}
