use bytes::Bytes;

use log::debug;
use log::error;

use tokio::sync::oneshot::channel;

use actix_web::{App, HttpServer};

use actix_web::web;
use actix_web::web::Data;
use actix_web::HttpRequest;
use actix_web::HttpResponse;

use sqlx::postgres::PgPoolOptions;

use openworkers_runner::store::WorkerIdentifier;

struct AppState {
    db: sqlx::Pool<sqlx::Postgres>,
}

async fn handle_request(data: Data<AppState>, req: HttpRequest, body: Bytes) -> HttpResponse {
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
            return HttpResponse::InternalServerError()
                .body("Failed to acquire a database connection");
        }
    };

    // Expect x-request-id header
    let request_id = match req.headers().get("x-request-id") {
        Some(value) => value.to_str().unwrap(),
        None => {
            return HttpResponse::BadRequest()
                .content_type("text/plain")
                .body("Missing request id");
        }
    };

    let host = match req.headers().get("host") {
        Some(value) => Some(value.to_str().unwrap().to_string()),
        None => None,
    };

    let mut worker_id = match req.headers().get("x-worker-id") {
        Some(value) => Some(value.to_str().unwrap().to_string()),
        None => None,
    };

    let mut worker_name = match req.headers().get("x-worker-name") {
        Some(value) => Some(value.to_str().unwrap().to_string()),
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
            return HttpResponse::BadRequest()
                .content_type("text/plain")
                .body("Missing worker id or name")
        }
    };

    let worker = openworkers_runner::store::get_worker(&mut conn, worker_identifier).await;

    debug!("worker found: {:?}", worker.is_some());

    let worker = match worker {
        Some(worker) => worker,
        None => {
            return HttpResponse::NotFound()
                .content_type("text/plain")
                .body("Worker not found");
        }
    };

    let start = tokio::time::Instant::now();

    // Create a new request to forward to the worker.
    let request = {
        let mut request: http_v02::Request<Bytes> = http_v02::Request::builder()
            .uri(format!(
                "{}://{}{}",
                req.connection_info().scheme(),
                req.connection_info().host(),
                req.uri()
            ))
            .method(req.method())
            .body(body)
            .unwrap();

        // Copy headers from the incoming request to the forwarded request.
        let headers = request.headers_mut();
        for (k, v) in req.headers() {
            headers.insert(k, v.clone());
        }

        // If the worker id is not provided, we add it to the headers.
        if req.headers().get("x-worker-id").is_none() {
            headers.insert(
                "x-worker-id",
                http_v02::HeaderValue::from_str(&worker.id).unwrap(),
            );
        }

        // If the worker name is not provided, we add it to the headers.
        if req.headers().get("x-worker-name").is_none() {
            headers.insert(
                "x-worker-name",
                http_v02::HeaderValue::from_str(&worker.name).unwrap(),
            );
        }

        request
    };

    let (res_tx, res_rx) = channel::<http_v02::Response<Bytes>>();

    let handle = openworkers_runner::event_fetch::run_fetch(worker, request, res_tx);

    // TODO: select! on res_rx, timeout and handle.join()
    let response = match res_rx.await {
        Ok(res) => {
            let mut rb = HttpResponse::build(res.status());

            for (k, v) in res.headers() {
                rb.append_header((k, v));
            }

            rb.body(res.body().clone())
        }
        Err(err) => {
            error!("worker fetch error: {}, ensure the worker registered a listener for the 'fetch' event", err);
            HttpResponse::InternalServerError().body(err.to_string())
        }
    };

    debug!("handle_request done in {}ms", start.elapsed().as_millis());

    handle.join().unwrap();

    response
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv::dotenv().ok();

    if !std::env::var("RUST_LOG").is_ok() {
        std::env::set_var("RUST_LOG", "info");
    }

    env_logger::init();

    debug!("start main");

    if !std::env::var("DATABASE_URL").is_ok() {
        let host = std::env::var("POSTGRES_HOST").expect("POSTGRES_HOST must be set");
        let port = std::env::var("POSTGRES_PORT").expect("POSTGRES_PORT must be set");
        let user = std::env::var("POSTGRES_USER").expect("POSTGRES_USER must be set");
        let password = std::env::var("POSTGRES_PASSWORD").expect("POSTGRES_PASSWORD must be set");
        let database = std::env::var("POSTGRES_DB").expect("POSTGRES_DB must be set");

        debug!("DATABASE_URL not set, using POSTGRES_* env vars");

        std::env::set_var(
            "DATABASE_URL",
            format!("postgres://{user}:{password}@{host}:{port}/{database}"),
        );
    }

    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&db_url)
        .await
        .expect("Failed to connect to Postgres");

    // Check postgres connection
    sqlx::query("SELECT 1")
        .fetch_one(&pool)
        .await
        .expect("Failed to query Postgres");
    debug!("connected to Postgres");

    // Check NATS connection
    openworkers_runner::nats::nats_connect()
        .publish("boot", "0")
        .expect("Failed to connect to NATS");
    debug!("connected to NATS");

    openworkers_runner::event_scheduled::handle_scheduled(pool.clone());

    HttpServer::new(move || {
        println!("Listening on http://localhost:8080");

        App::new()
            .app_data(Data::new(AppState { db: pool.clone() }))
            .service(
                web::resource("/health")
                    .guard(actix_web::guard::Header("host", "127.0.0.1:8080"))
                    .route(web::head().to(HttpResponse::Ok))
                    .route(web::get().to(HttpResponse::Ok)),
            )
            .default_service(web::to(handle_request))
    })
    .bind(("0.0.0.0", 8080))?
    .workers(4)
    .run()
    .await
}
