//! The listener of `d1_requests` serves requests until the runner stops it.
//!
//! This test needs a database with the openworkers-cli migrations:
//! `TEST_DATABASE_URL=postgres://... cargo test --features v8 --test d1_requests_postgres_test -- --ignored`

use std::time::Duration;

use openworkers_runner::d1::{Config, Limits, Service};
use tokio_util::sync::CancellationToken;

async fn finished(pool: &sqlx::PgPool, id: uuid::Uuid) -> bool {
    sqlx::query_scalar("SELECT finished_at IS NOT NULL FROM d1_requests WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn request(pool: &sqlx::PgPool, database_id: uuid::Uuid, sql: &str) -> uuid::Uuid {
    let request = serde_json::json!({ "type": "op", "op": { "kind": "exec", "sql": sql } });

    sqlx::query_scalar(
        "INSERT INTO d1_requests (database_id, request) VALUES ($1, $2) RETURNING id",
    )
    .bind(database_id)
    .bind(request)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn the_listener_stops_between_requests() {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL is not set");
    let pool = sqlx::PgPool::connect(&url).await.unwrap();

    let dir = std::env::temp_dir().join(format!("d1-requests-test-{}", uuid::Uuid::new_v4()));
    let service: &'static Service = Box::leak(Box::new(
        Service::new(Config {
            data_dir: dir.clone(),
            limits: Limits::default(),
            timeout: Duration::from_secs(5),
            max_queue: 64,
            max_open: 16,
            idle: Duration::from_secs(300),
            min_free_bytes: 0,
            namespace_bytes: u64::MAX,
        })
        .unwrap(),
    ));

    let user_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let database_id: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO d1_databases (user_id, name) VALUES ($1, $2) RETURNING id")
            .bind(user_id)
            .bind(format!("requests-test-{}", uuid::Uuid::new_v4()))
            .fetch_one(&pool)
            .await
            .unwrap();

    let stop = CancellationToken::new();
    let listener = tokio::spawn(openworkers_runner::d1::requests::listen(
        service,
        url.clone(),
        stop.clone(),
    ));

    let served = request(&pool, database_id, "CREATE TABLE t (a)").await;

    for _ in 0..100 {
        if finished(&pool, served).await {
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        finished(&pool, served).await,
        "the listener serves a request"
    );

    // Once stopped, the listener ends, and a later request waits
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), listener)
        .await
        .expect("the listener ends once stopped")
        .unwrap();

    let waiting = request(&pool, database_id, "INSERT INTO t VALUES (1)").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!finished(&pool, waiting).await);

    sqlx::query("DELETE FROM d1_databases WHERE id = $1")
        .bind(database_id)
        .execute(&pool)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(dir);
}
