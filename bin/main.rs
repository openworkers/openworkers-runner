use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use log::{debug, error, warn};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpSocket;
use tokio::sync::oneshot::channel;

use openworkers_core::{HttpRequest, HyperBody};
use openworkers_runner::store::WorkerIdentifier;

use sqlx::postgres::PgPoolOptions;

struct AppState {
    db: sqlx::Pool<sqlx::Postgres>,
    log_tx: std::sync::mpsc::Sender<openworkers_runner::log::LogMessage>,
}

async fn handle_request(
    state: &AppState,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<HyperBody>, std::convert::Infallible> {
    // Health check endpoint (only for local requests, not worker domains)
    if req.uri().path() == "/health" {
        let is_local = req
            .headers()
            .get("host")
            .and_then(|h| h.to_str().ok())
            .map(|h| h == "127.0.0.1:8080")
            .unwrap_or(false);

        if is_local {
            return Ok(Response::builder()
                .status(200)
                .body(HyperBody::Full(http_body_util::Full::new(Bytes::from(
                    "OK",
                ))))
                .unwrap());
        }
    }

    debug!(
        "handle_request: {} {} in thread {:?}",
        req.method(),
        req.uri(),
        std::thread::current().id()
    );

    // Extract parts before consuming the body
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

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

    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let mut worker_id = headers
        .get("x-worker-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let mut worker_name = headers
        .get("x-worker-name")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    debug!(
        "host: {:?}, worker_id: {:?}, worker_name: {:?}",
        host, worker_id, worker_name
    );

    // Resolve worker from domain if not provided
    if worker_id.is_none() && worker_name.is_none() {
        if let Some(ref host) = host {
            if host.contains(".workers.") {
                worker_name = Some(host.split('.').next().unwrap().to_string());
            } else {
                worker_id =
                    openworkers_runner::store::get_worker_id_from_domain(&mut conn, host.clone())
                        .await;
            }
        }
    }

    debug!(
        "request_id: {}, worker_id: {:?}, worker_name: {:?}",
        request_id, worker_id, worker_name
    );

    let worker_identifier = match (worker_id, worker_name) {
        (Some(id), _) => WorkerIdentifier::Id(id),
        (None, Some(name)) => WorkerIdentifier::Name(name),
        _ => return Ok(error_response(400, "Missing worker id or name")),
    };

    let worker =
        openworkers_runner::store::get_worker_with_bindings(&mut conn, worker_identifier).await;

    debug!("worker found: {:?}", worker.is_some());

    let worker = match worker {
        Some(worker) => worker,
        None => return Ok(error_response(404, "Worker not found")),
    };

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
        request
            .headers
            .insert("x-worker-name".to_string(), worker.name.clone());
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

    let (res_tx, res_rx) = channel::<openworkers_runner::runtime::HttpResponse>();
    let (termination_tx, termination_rx) =
        channel::<Result<(), openworkers_core::TerminationReason>>();

    // Create disconnect notification channel
    let (disconnect_tx, _disconnect_rx) = channel::<()>();

    openworkers_runner::event_fetch::run_fetch(
        worker,
        request,
        res_tx,
        termination_tx,
        state.log_tx.clone(),
        permit,
        state.db.clone(),
    );

    // TODO: Pass disconnect_rx to the worker so it can stop processing

    let response = match res_rx.await {
        Ok(res) => {
            // Convert to hyper Response with disconnect notification
            res.into_hyper_with_disconnect(Some(disconnect_tx))
        }
        Err(_) => {
            // Worker didn't send response, check termination reason
            use openworkers_core::TerminationReason;

            let result = termination_rx
                .await
                .unwrap_or(Err(TerminationReason::Other("Unknown error".to_string())));

            match result {
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
            }
        }
    };

    debug!("handle_request done in {}ms", start.elapsed().as_millis());

    Ok(response)
}

fn error_response(status: u16, message: &str) -> Response<HyperBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(HyperBody::Full(http_body_util::Full::new(Bytes::from(
            message.to_string(),
        ))))
        .unwrap()
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    env_logger::init();

    debug!("start main (hyper)");

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

    // Retry database connection with exponential backoff
    let mut retry_count = 0;
    let max_retries = 5;
    let pool = loop {
        match PgPoolOptions::new()
            .max_connections(20)
            .acquire_timeout(Duration::from_secs(5))
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

    openworkers_runner::event_scheduled::handle_scheduled(pool.clone(), log_tx.clone());

    let state = std::sync::Arc::new(AppState { db: pool, log_tx });

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let num_listeners = 4;

    println!(
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

    // Keep main alive
    std::future::pending::<()>().await;

    Ok(())
}
