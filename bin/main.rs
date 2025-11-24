use bytes::Bytes;

use log::{debug, error, warn};

use std::time::Duration;

use tokio::sync::oneshot::channel;

use actix_web::{App, HttpServer, web, web::Data};

use sqlx::postgres::PgPoolOptions;

use openworkers_runner::store::WorkerIdentifier;

struct AppState {
    db: sqlx::Pool<sqlx::Postgres>,
    log_tx: std::sync::mpsc::Sender<openworkers_runner::log::LogMessage>,
}

async fn handle_request(
    data: Data<AppState>,
    req: actix_web::HttpRequest,
    body: Bytes,
) -> actix_web::HttpResponse {
    debug!(
        "handle_request of: {} {} in thread {:?}",
        req.method(),
        req.uri(),
        std::thread::current().id()
    );

    // Acquire a database connection from the pool.
    let mut conn: sqlx::pool::PoolConnection<sqlx::Postgres> = match data.db.acquire().await {
        Ok(db) => db,
        Err(err) => {
            error!("Failed to acquire a database connection: {}", err);
            return actix_web::HttpResponse::InternalServerError()
                .body("Failed to acquire a database connection");
        }
    };

    // Expect x-request-id header
    let request_id = match req.headers().get("x-request-id") {
        Some(value) => match value.to_str() {
            Ok(s) => s,
            Err(_) => {
                return actix_web::HttpResponse::BadRequest()
                    .content_type("text/plain")
                    .body("Invalid x-request-id header encoding");
            }
        },
        None => {
            return actix_web::HttpResponse::BadRequest()
                .content_type("text/plain")
                .body("Missing request id");
        }
    };

    let host = match req.headers().get("host") {
        Some(value) => match value.to_str() {
            Ok(s) => Some(s.to_string()),
            Err(_) => {
                error!("Invalid host header encoding");
                None
            }
        },
        None => None,
    };

    let mut worker_id = match req.headers().get("x-worker-id") {
        Some(value) => match value.to_str() {
            Ok(s) => Some(s.to_string()),
            Err(_) => {
                error!("Invalid x-worker-id header encoding");
                None
            }
        },
        None => None,
    };

    let mut worker_name = match req.headers().get("x-worker-name") {
        Some(value) => match value.to_str() {
            Ok(s) => Some(s.to_string()),
            Err(_) => {
                error!("Invalid x-worker-name header encoding");
                None
            }
        },
        None => None,
    };

    debug!(
        "host: {:?}, worker_id: {:?}, worker_name: {:?}",
        host, worker_id, worker_name
    );

    // If the worker id is not provided, we try to get it from the domain name.
    if worker_id.is_none() && worker_name.is_none() && host.is_some() {
        // If the host is a *.workers.* domain, we can get the worker name from the host.
        // Local: name.workers.dev.localhost
        // Prod: name.workers.rocks
        if let Some(host) = host {
            if host.contains(".workers.") {
                worker_name = Some(host.split('.').next().unwrap().to_string());
            } else {
                worker_id =
                    openworkers_runner::store::get_worker_id_from_domain(&mut conn, host).await;
            }
        }
    }

    debug!(
        "request_id: {request_id}, worker_id: {:?}, worker_name: {:?}",
        worker_id, worker_name
    );

    let worker_identifier = match (worker_id, worker_name) {
        (Some(id), _) => WorkerIdentifier::Id(id),
        (None, Some(name)) => WorkerIdentifier::Name(name),
        // If we don't have a worker id or name, we can't continue.
        _ => {
            return actix_web::HttpResponse::BadRequest()
                .content_type("text/plain")
                .body("Missing worker id or name");
        }
    };

    let worker = openworkers_runner::store::get_worker(&mut conn, worker_identifier).await;

    debug!("worker found: {:?}", worker.is_some());

    let worker = match worker {
        Some(worker) => worker,
        None => {
            return actix_web::HttpResponse::NotFound()
                .content_type("text/plain")
                .body("Worker not found");
        }
    };

    let start = tokio::time::Instant::now();

    // Create a new request to forward to the worker.
    let mut request = openworkers_runtime::HttpRequest::from_actix(&req, body);

    // If the worker id is not provided, we add it to the headers.
    if req.headers().get("x-worker-id").is_none() {
        request
            .headers
            .insert("x-worker-id".to_string(), worker.id.clone());
    }

    // If the worker name is not provided, we add it to the headers.
    if req.headers().get("x-worker-name").is_none() {
        request
            .headers
            .insert("x-worker-name".to_string(), worker.name.clone());
    }

    // Try to acquire a worker slot from the semaphore with timeout
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
            return actix_web::HttpResponse::InternalServerError()
                .content_type("text/plain")
                .body("Internal server error");
        }
        Err(_) => {
            debug!(
                "worker pool saturated after {}ms timeout, returning 503",
                timeout.as_millis()
            );
            return actix_web::HttpResponse::ServiceUnavailable()
                .content_type("text/plain")
                .body("Server is overloaded, please try again later");
        }
    };

    let (res_tx, res_rx) = channel::<openworkers_runtime::HttpResponse>();
    let (termination_tx, termination_rx) = channel::<openworkers_runner::TerminationReason>();

    openworkers_runner::event_fetch::run_fetch(
        worker,
        request,
        res_tx,
        termination_tx,
        data.log_tx.clone(),
        permit,
    );

    let response = match res_rx.await {
        Ok(res) => {
            // Convert our HttpResponse to actix_web::HttpResponse
            res.into()
        }
        Err(_) => {
            // Worker didn't send a response, check termination reason
            use openworkers_runner::TerminationReason;

            let reason = termination_rx.await.unwrap_or(TerminationReason::Exception);

            error!("worker terminated without sending response: {:?}", reason);

            let status = reason.http_status();
            let body = match reason {
                TerminationReason::Success => {
                    // This shouldn't happen - worker completed but didn't send response
                    "Worker completed but did not send a response (missing fetch event listener?)"
                }
                TerminationReason::CpuTimeLimit => "Worker exceeded CPU time limit (100ms)",
                TerminationReason::WallClockTimeout => {
                    "Worker exceeded wall-clock time limit (60s)"
                }
                TerminationReason::MemoryLimit => "Worker exceeded memory limit (128MB)",
                TerminationReason::Exception => "Worker threw an uncaught exception",
                TerminationReason::InitializationError => "Worker failed to initialize",
                TerminationReason::Terminated => "Worker was terminated",
            };

            actix_web::HttpResponse::build(actix_web::http::StatusCode::from_u16(status).unwrap())
                .content_type("text/plain")
                .insert_header(("X-Termination-Reason", format!("{:?}", reason)))
                .body(body)
        }
    };

    debug!("handle_request done in {}ms", start.elapsed().as_millis());

    response
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv::dotenv().ok();

    env_logger::init();

    debug!("start main");

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
            .max_connections(20) // Increased from 4
            .acquire_timeout(Duration::from_secs(5))
            .connect(&db_url)
            .await
        {
            Ok(pool) => {
                // Test the connection
                match sqlx::query("SELECT 1").fetch_one(&pool).await {
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
                }
            }
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

    HttpServer::new(move || {
        println!("Listening on http://localhost:8080");

        App::new()
            .app_data(Data::new(AppState {
                db: pool.clone(),
                log_tx: log_tx.clone(),
            }))
            .service(
                web::resource("/health")
                    .guard(actix_web::guard::Header("host", "127.0.0.1:8080"))
                    .route(web::head().to(actix_web::HttpResponse::Ok))
                    .route(web::get().to(actix_web::HttpResponse::Ok)),
            )
            .default_service(web::to(handle_request))
    })
    .bind(("0.0.0.0", 8080))?
    .workers(4)
    .run()
    .await
}
