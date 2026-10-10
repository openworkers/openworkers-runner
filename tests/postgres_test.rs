//! The parts of the runner that use the platform database: the logs table,
//! the cron claims and the runner lock.
//!
//! These tests need a database with the openworkers-cli migrations:
//! `TEST_DATABASE_URL=postgres://... cargo test --features v8 --test postgres_test -- --ignored`

use chrono::{DateTime, Duration, DurationRound, Utc};
use openworkers_core::{LogEvent, LogLevel};
use openworkers_runner::log::LogSink;
use openworkers_runner::{logs, scheduler, singleton};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn database_url() -> String {
    std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL is not set")
}

async fn database() -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .unwrap()
}

/// A new user with one worker; gives the worker id.
async fn new_worker(db: &PgPool) -> Uuid {
    let user: Uuid = sqlx::query_scalar("INSERT INTO users (username) VALUES ($1) RETURNING id")
        .bind(format!("test-{}", Uuid::new_v4().simple()))
        .fetch_one(db)
        .await
        .unwrap();

    sqlx::query_scalar("INSERT INTO workers (user_id) VALUES ($1) RETURNING id")
        .bind(user)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Sends the lines through a sink and waits until the store wrote them.
async fn store_lines(db: &PgPool, lines: Vec<(String, LogLevel, String)>) {
    let (sink, store) = LogSink::new();
    let stop = CancellationToken::new();
    let task = tokio::spawn(logs::store(db.clone(), store, stop.clone()));

    for (worker_id, level, message) in lines {
        sink.send(&worker_id, LogEvent { level, message });
    }

    stop.cancel();
    task.await.unwrap();
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn stored_lines_keep_their_level_and_255_characters() {
    let db = database().await;
    let worker = new_worker(&db).await;

    store_lines(
        &db,
        vec![
            (worker.to_string(), LogLevel::Error, "é".repeat(300)),
            (worker.to_string(), LogLevel::Warn, "second".to_string()),
            // Neither line has a worker row; they must not fail the batch
            (
                "slot-worker".to_string(),
                LogLevel::Log,
                "no uuid".to_string(),
            ),
            (
                Uuid::new_v4().to_string(),
                LogLevel::Log,
                "deleted".to_string(),
            ),
        ],
    )
    .await;

    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT level::text, message FROM logs WHERE worker_id = $1 ORDER BY date")
            .bind(worker)
            .fetch_all(&db)
            .await
            .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "error");
    assert_eq!(rows[0].1, "é".repeat(255));
    assert_eq!(rows[1], ("warn".to_string(), "second".to_string()));
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_history_is_the_last_ten_lines_oldest_first() {
    let db = database().await;
    let worker = new_worker(&db).await;

    let lines = (0..12)
        .map(|i| (worker.to_string(), LogLevel::Info, i.to_string()))
        .collect();

    store_lines(&db, lines).await;

    let history = logs::history(&db, worker).await.unwrap();
    let messages: Vec<_> = history.iter().map(|entry| entry.message.as_str()).collect();

    assert_eq!(
        messages,
        ["2", "3", "4", "5", "6", "7", "8", "9", "10", "11"]
    );
    assert!(history.iter().all(|entry| entry.level == LogLevel::Info));
    assert!(
        history
            .iter()
            .all(|entry| entry.worker_id == worker.to_string())
    );
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_store_writes_its_queue_when_the_runner_stops() {
    let db = database().await;
    let worker = new_worker(&db).await;

    let lines = (0..2000)
        .map(|i| (worker.to_string(), LogLevel::Log, i.to_string()))
        .collect();

    store_lines(&db, lines).await;

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM logs WHERE worker_id = $1")
        .bind(worker)
        .fetch_one(&db)
        .await
        .unwrap();

    assert_eq!(count, 2000);
}

/// A claim takes every due cron of the table, so the cron tests run one at a
/// time, or one test claims the crons of another.
static CLAIMS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Removes the user of the worker, with its crons, events and logs, so the
/// crons of old runs do not fill the claims of new runs.
async fn remove_worker(db: &PgPool, worker: Uuid) {
    sqlx::query("DELETE FROM users WHERE id = (SELECT user_id FROM workers WHERE id = $1)")
        .bind(worker)
        .execute(db)
        .await
        .unwrap();
}

/// Postgres keeps microseconds; the claims compare dates for equality.
fn now() -> DateTime<Utc> {
    Utc::now()
        .duration_trunc(Duration::microseconds(1))
        .unwrap()
}

async fn new_cron(db: &PgPool, worker: Uuid, pattern: &str, next_run: DateTime<Utc>) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO crons (worker_id, value, next_run) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(worker)
    .bind(pattern)
    .bind(next_run)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn cron_runs(db: &PgPool, cron: Uuid) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    sqlx::query_as("SELECT last_run, next_run FROM crons WHERE id = $1")
        .bind(cron)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn events(db: &PgPool, cron: Uuid) -> Vec<(Uuid, DateTime<Utc>)> {
    sqlx::query_as("SELECT id, scheduled_at FROM scheduled_events WHERE cron_id = $1")
        .bind(cron)
        .fetch_all(db)
        .await
        .unwrap()
}

/// Claims the due crons; gives the events of `worker`, as other tests have
/// crons in the same table.
async fn claim(db: &PgPool, worker: Uuid, at: DateTime<Utc>) -> Vec<String> {
    scheduler::claim_due(db, at)
        .await
        .unwrap()
        .into_iter()
        .filter(|data| data.worker_id == worker.to_string())
        .map(|data| data.id)
        .collect()
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_late_cron_runs_once_then_waits_for_its_next_run() {
    let _claims = CLAIMS.lock().await;
    let db = database().await;
    let worker = new_worker(&db).await;
    let now = now();
    let late = now - Duration::hours(3);
    let cron = new_cron(&db, worker, "* * * * *", late).await;

    let claimed = claim(&db, worker, now).await;

    assert_eq!(claimed.len(), 1);

    let (last_run, next_run) = cron_runs(&db, cron).await;
    assert_eq!(last_run, Some(late));
    assert_eq!(next_run, scheduler::next_run("* * * * *", now));
    assert!(next_run.unwrap() > now);

    let recorded = events(&db, cron).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0.to_string(), claimed[0]);
    assert_eq!(recorded[0].1, late);

    assert!(claim(&db, worker, now).await.is_empty());

    remove_worker(&db, worker).await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn two_claims_at_the_same_time_start_one_event() {
    let _claims = CLAIMS.lock().await;
    let db = database().await;
    let worker = new_worker(&db).await;
    let now = now();
    let cron = new_cron(&db, worker, "*/5 * * * * *", now - Duration::seconds(1)).await;

    let claims = futures::future::join_all((0..8).map(|_| claim(&db, worker, now))).await;

    assert_eq!(claims.iter().map(Vec::len).sum::<usize>(), 1);
    assert_eq!(events(&db, cron).await.len(), 1);

    remove_worker(&db, worker).await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_pattern_that_is_not_valid_stops_its_cron() {
    let _claims = CLAIMS.lock().await;
    let db = database().await;
    let worker = new_worker(&db).await;
    let now = now();
    let cron = new_cron(&db, worker, "61 * * * *", now - Duration::minutes(1)).await;

    assert!(claim(&db, worker, now).await.is_empty());
    assert_eq!(cron_runs(&db, cron).await, (None, None));
    assert!(events(&db, cron).await.is_empty());

    remove_worker(&db, worker).await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn deleted_and_future_crons_do_not_run() {
    let _claims = CLAIMS.lock().await;
    let db = database().await;
    let worker = new_worker(&db).await;
    let now = now();
    let future = now + Duration::minutes(5);

    let waiting = new_cron(&db, worker, "* * * * *", future).await;
    let deleted = new_cron(&db, worker, "* * * * *", now - Duration::minutes(1)).await;

    sqlx::query("UPDATE crons SET deleted_at = NOW() WHERE id = $1")
        .bind(deleted)
        .execute(&db)
        .await
        .unwrap();

    assert!(claim(&db, worker, now).await.is_empty());
    assert_eq!(cron_runs(&db, waiting).await, (None, Some(future)));
    assert!(events(&db, deleted).await.is_empty());

    remove_worker(&db, worker).await;
}

/// One test, as the lock is the same for every test of the database.
#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn one_runner_holds_the_lock_until_its_session_ends() {
    let db = database().await;
    let url = database_url();

    let first = singleton::acquire(&url).await.unwrap();
    assert!(singleton::acquire(&url).await.is_err());

    drop(first);
    // The server ends the session after the client closes it
    let second = wait_for_lock(&url).await;

    let pid: i32 = sqlx::query_scalar(
        "SELECT pid FROM pg_locks WHERE locktype = 'advisory' \
         AND classid = $1::int4::oid AND objid = $2::int4::oid AND objsubid = 2 AND granted",
    )
    .bind(singleton::LOCK_KEYS.0)
    .bind(singleton::LOCK_KEYS.1)
    .fetch_one(&db)
    .await
    .unwrap();

    let hold = tokio::spawn(singleton::hold(second));

    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .execute(&db)
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(10), hold)
        .await
        .expect("hold returns when the session of the lock ends")
        .unwrap();

    drop(wait_for_lock(&url).await);
}

async fn wait_for_lock(url: &str) -> sqlx::PgConnection {
    for _ in 0..50 {
        if let Ok(connection) = singleton::acquire(url).await {
            return connection;
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    panic!("the lock stayed taken for 5 s");
}
