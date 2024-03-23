use bytes::Bytes;

use log::debug;
use log::error;
use openworkers_runtime::FetchInit;
use openworkers_runtime::Script;
use openworkers_runtime::Task;
use openworkers_runtime::Worker;

use tokio::sync::oneshot::channel;

use actix_web::{App, HttpServer};

use actix_web::web;
use actix_web::web::Data;
use actix_web::HttpRequest;
use actix_web::HttpResponse;

use sqlx::postgres::PgPoolOptions;

use openworkers_runner::store::WorkerIdentifier;

type Database = sqlx::Pool<sqlx::Postgres>;

struct AppState {
    db: Database,
}

async fn handle_request(data: Data<AppState>, req: HttpRequest) -> HttpResponse {
    debug!(
        "handle_request of: {} {} in thread {:?}",
        req.method(),
        req.uri(),
        std::thread::current().id()
    );

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
                    openworkers_runner::store::get_worker_id_from_domain(&data.db, host).await;
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

    let worker = openworkers_runner::store::get_worker(&data.db, worker_identifier).await;

    debug!("worker found: {:?}", worker.is_some());

    let worker = match worker {
        Some(worker) => worker,
        None => {
            return HttpResponse::NotFound()
                .content_type("text/plain")
                .body("Worker not found");
        }
    };

    let log_tx = openworkers_runner::log::create_log_handler(worker.id);

    let script = Script {
        specifier: openworkers_runtime::module_url("script.js"),
        code: Some(openworkers_runtime::FastString::from(worker.script)),
        env: match worker.env {
            Some(env) => Some(env.encode_to_string()),
            None => None,
        },
    };

    let start = tokio::time::Instant::now();

    let req = http_v02::Request::builder()
        .uri(req.uri())
        .body(Default::default())
        .unwrap();

    let (res_tx, res_rx) = channel::<http_v02::Response<Bytes>>();
    let task = Task::Fetch(Some(FetchInit::new(req, res_tx)));

    let handle = std::thread::spawn(move || {
        let local = tokio::task::LocalSet::new();

        let tasks = local.spawn_local(async move {
            debug!("create worker");
            let mut worker = Worker::new(script, Some(log_tx)).await.unwrap();

            debug!("exec fetch task");
            match worker.exec(task).await {
                Ok(()) => debug!("exec completed"),
                Err(err) => error!("exec did not complete: {err}"),
            }
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        match local.block_on(&rt, tasks) {
            Ok(()) => {}
            Err(err) => error!("failed to wait for end: {err}"),
        }
    });

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

async fn health_check() -> HttpResponse {
    HttpResponse::Ok().body("ok")
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

        std::env::set_var("DATABASE_URL", format!("postgres://{user}:{password}@{host}:{port}/{database}"));
    }

    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&db_url)
        .await
        .expect("Failed to connect to Postgres");

    openworkers_runner::scheduled::handle_scheduled(pool.clone());

    HttpServer::new(move || {
        println!("Listening on http://localhost:8080");

        App::new()
            .app_data(Data::new(AppState { db: pool.clone() }))
            .route("/health", web::get().to(health_check))
            .default_service(web::to(handle_request))
    })
    .bind(("127.0.0.1", 8080))?
    .workers(4)
    .run()
    .await
}
