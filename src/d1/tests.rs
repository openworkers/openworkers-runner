use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use openworkers_core::D1ErrorKind;
use openworkers_core::D1Op;
use openworkers_core::D1Result;
use openworkers_core::D1Rows;
use openworkers_core::D1Statement;
use openworkers_core::D1StatementResult;
use openworkers_core::SqlPrimitive;

use super::Config;
use super::Limits;
use super::Service;

const DB: &str = "11111111-1111-4111-8111-111111111111";
const OTHER_DB: &str = "22222222-2222-4222-8222-222222222222";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("d1-test-{}", uuid::Uuid::new_v4()));

        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(dir: &TempDir) -> Config {
    Config {
        data_dir: dir.0.clone(),
        limits: Limits::default(),
        timeout: Duration::from_secs(5),
        max_queue: 64,
        max_open: 16,
        idle: Duration::from_secs(300),
    }
}

fn statement(sql: &str, params: Vec<SqlPrimitive>) -> D1Statement {
    D1Statement {
        sql: sql.to_string(),
        params,
    }
}

async fn query(service: &Service, sql: &str, params: Vec<SqlPrimitive>) -> D1Result {
    let op = D1Op::Query {
        statement: statement(sql, params),
        rows: D1Rows::All,
    };

    service.execute(DB, op).await
}

async fn exec(service: &Service, sql: &str) -> D1Result {
    service.execute(DB, D1Op::Exec { sql: sql.into() }).await
}

fn rows(result: D1Result) -> D1StatementResult {
    match result {
        D1Result::Query(result) => result,
        other => panic!("expected a query result, got {other:?}"),
    }
}

/// The error kind and message, for an error result
fn failure(result: D1Result) -> (D1ErrorKind, String, Option<usize>) {
    match result {
        D1Result::Error(err) => (err.kind, err.message, err.statement),
        other => panic!("expected an error, got {other:?}"),
    }
}

fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap()
}

async fn count(service: &Service, table: &str) -> String {
    let result = rows(
        query(
            service,
            &format!("SELECT count(*) AS n FROM {table}"),
            vec![],
        )
        .await,
    );

    json(&result.rows)
}

#[tokio::test]
async fn values_round_trip() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    let result = rows(
        query(
            &service,
            "SELECT ?1 AS i, ?2 AS f, ?3 AS whole, ?4 AS t, ?5 AS b, ?6 AS n, ?7 AS yes, \
             typeof(?3) AS whole_type, typeof(?7) AS yes_type",
            vec![
                SqlPrimitive::Int(-7),
                SqlPrimitive::Float(1.5),
                SqlPrimitive::Float(4_294_967_296.0),
                SqlPrimitive::String("text".into()),
                SqlPrimitive::Bytes(vec![0, 255]),
                SqlPrimitive::Null,
                SqlPrimitive::Bool(true),
            ],
        )
        .await,
    );

    assert_eq!(
        json(&result.columns),
        r#"["i","f","whole","t","b","n","yes","whole_type","yes_type"]"#
    );
    assert_eq!(
        json(&result.rows),
        r#"[[-7,1.5,4294967296,"text",{"$bytes":"AP8="},null,1,"integer","integer"]]"#
    );
}

#[tokio::test]
async fn numbered_and_anonymous_parameters() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    let numbered = rows(
        query(
            &service,
            "SELECT ?2, ?1",
            vec![SqlPrimitive::Int(1), SqlPrimitive::Int(2)],
        )
        .await,
    );
    let anonymous = rows(
        query(
            &service,
            "SELECT ?, ?",
            vec![SqlPrimitive::Int(1), SqlPrimitive::Int(2)],
        )
        .await,
    );

    assert_eq!(json(&numbered.rows), "[[2,1]]");
    assert_eq!(json(&anonymous.rows), "[[1,2]]");
}

#[tokio::test]
async fn parameter_count_and_names_are_checked() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    let (kind, message, _) =
        failure(query(&service, "SELECT ?1, ?2", vec![SqlPrimitive::Int(1)]).await);

    assert_eq!(kind, D1ErrorKind::Range);
    assert!(message.contains("2 expected, 1 given"), "{message}");

    let (kind, message, _) =
        failure(query(&service, "SELECT :name", vec![SqlPrimitive::Int(1)]).await);

    assert_eq!(kind, D1ErrorKind::Sql);
    assert!(message.contains("named parameter :name"), "{message}");
}

#[tokio::test]
async fn sql_errors_have_the_d1_form() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    let (kind, message, _) = failure(query(&service, "SELECT * FROM missing", vec![]).await);

    assert_eq!(kind, D1ErrorKind::Sql);
    assert_eq!(message, "no such table: missing: SQLITE_ERROR");

    let (_, message, _) = failure(query(&service, "SELEC 1", vec![]).await);

    assert_eq!(message, "near \"SELEC\": syntax error: SQLITE_ERROR");

    let (_, message, _) = failure(query(&service, "SELECT load_extension('x')", vec![]).await);

    assert_eq!(
        message,
        "not authorized to use function: load_extension: SQLITE_ERROR"
    );

    exec(&service, "CREATE TABLE t (a INTEGER UNIQUE)").await;
    query(&service, "INSERT INTO t VALUES (1)", vec![]).await;

    let (_, message, _) = failure(query(&service, "INSERT INTO t VALUES (1)", vec![]).await);

    assert_eq!(message, "UNIQUE constraint failed: t.a: SQLITE_CONSTRAINT");
}

#[tokio::test]
async fn meta_reports_changes_and_size() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    exec(&service, "CREATE TABLE t (a)").await;

    let insert = rows(query(&service, "INSERT INTO t VALUES (1), (2)", vec![]).await);
    let select = rows(query(&service, "SELECT * FROM t", vec![]).await);

    assert_eq!(insert.meta.changes, 2);
    assert_eq!(insert.meta.last_row_id, 2);
    assert!(insert.meta.changed_db);
    assert!(insert.meta.size_after > 0);
    assert_eq!(select.meta.changes, 0);
    assert!(!select.meta.changed_db);
}

#[tokio::test]
async fn batch_rolls_back_on_failure() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    exec(&service, "CREATE TABLE t (a INTEGER UNIQUE)").await;

    let insert = |value| statement("INSERT INTO t VALUES (?1)", vec![SqlPrimitive::Int(value)]);

    let op = D1Op::Batch {
        statements: vec![insert(1), insert(2), insert(1)],
    };
    let (kind, _, index) = failure(service.execute(DB, op).await);

    assert_eq!(kind, D1ErrorKind::Sql);
    assert_eq!(index, Some(2));
    assert_eq!(count(&service, "t").await, "[[0]]");

    let op = D1Op::Batch {
        statements: vec![insert(1), insert(2)],
    };

    assert!(
        matches!(service.execute(DB, op).await, D1Result::Batch { results } if results.len() == 2)
    );
    assert_eq!(count(&service, "t").await, "[[2]]");
}

#[tokio::test]
async fn exec_stops_at_the_first_error() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    let (kind, _, index) = failure(
        exec(
            &service,
            "CREATE TABLE t (a TEXT);\nINSERT INTO t VALUES ('x;y');\nINSERT INTO nope VALUES (1);\nINSERT INTO t VALUES ('z');",
        )
        .await,
    );

    assert_eq!(kind, D1ErrorKind::Sql);
    assert_eq!(index, Some(2));

    let result = rows(query(&service, "SELECT a FROM t", vec![]).await);

    assert_eq!(json(&result.rows), r#"[["x;y"]]"#);
}

#[tokio::test]
async fn exec_counts_statements_over_lines() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    let result = exec(
        &service,
        "CREATE TABLE t (\n  a TEXT\n);\n-- a comment\nINSERT INTO t VALUES (';');",
    )
    .await;

    assert!(
        matches!(result, D1Result::Exec { count: 2, .. }),
        "{result:?}"
    );
}

#[tokio::test]
async fn exec_rejects_parameters() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    let (_, message, _) = failure(exec(&service, "SELECT ?").await);

    assert!(message.contains("does not take parameters"), "{message}");
}

#[tokio::test]
async fn first_still_runs_a_write_to_its_end() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    exec(&service, "CREATE TABLE t (a)").await;

    let op = D1Op::Query {
        statement: statement("INSERT INTO t VALUES (1), (2), (3) RETURNING a", vec![]),
        rows: D1Rows::First,
    };
    let result = rows(service.execute(DB, op).await);

    assert_eq!(json(&result.rows), "[[1]]");
    assert_eq!(count(&service, "t").await, "[[3]]");
}

#[tokio::test]
async fn guest_cannot_escape_the_database() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();
    let outside = dir.0.join("outside.sqlite");

    for sql in [
        format!("ATTACH '{}' AS other", outside.display()),
        format!("VACUUM INTO '{}'", outside.display()),
        "BEGIN".to_string(),
        "SAVEPOINT s".to_string(),
        "PRAGMA journal_mode = DELETE".to_string(),
        "PRAGMA foreign_keys = OFF".to_string(),
        "PRAGMA writable_schema = ON".to_string(),
        "SELECT load_extension('x')".to_string(),
    ] {
        let (kind, message, _) = failure(query(&service, &sql, vec![]).await);

        assert_eq!(kind, D1ErrorKind::Sql, "{sql}: {message}");
    }

    assert!(!outside.exists());

    exec(&service, "CREATE TABLE t (a)").await;

    let info = rows(query(&service, "PRAGMA table_info(t)", vec![]).await);

    assert_eq!(info.rows.len(), 1);
}

#[tokio::test]
async fn foreign_keys_are_enforced() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    exec(
        &service,
        "CREATE TABLE parent (id INTEGER PRIMARY KEY);\
         CREATE TABLE child (parent INTEGER REFERENCES parent (id));",
    )
    .await;

    let (_, message, _) = failure(query(&service, "INSERT INTO child VALUES (1)", vec![]).await);

    assert_eq!(message, "FOREIGN KEY constraint failed: SQLITE_CONSTRAINT");
}

#[tokio::test]
async fn databases_are_isolated() {
    let dir = TempDir::new();
    let service = Service::new(config(&dir)).unwrap();

    exec(&service, "CREATE TABLE secret (a)").await;

    let op = D1Op::Query {
        statement: statement("SELECT * FROM secret", vec![]),
        rows: D1Rows::All,
    };
    let (_, message, _) = failure(service.execute(OTHER_DB, op).await);

    assert_eq!(message, "no such table: secret: SQLITE_ERROR");

    let op = D1Op::Exec {
        sql: "SELECT 1".into(),
    };
    let (kind, _, _) = failure(service.execute("../escape", op).await);

    assert_eq!(kind, D1ErrorKind::Unavailable);
}

#[tokio::test]
async fn data_survives_a_restart() {
    let dir = TempDir::new();

    {
        let service = Service::new(config(&dir)).unwrap();

        exec(&service, "CREATE TABLE t (a); INSERT INTO t VALUES (1);").await;
    }

    let service = Service::new(config(&dir)).unwrap();

    assert_eq!(count(&service, "t").await, "[[1]]");
}

#[tokio::test]
async fn one_process_holds_the_data_directory() {
    let dir = TempDir::new();
    let _service = Service::new(config(&dir)).unwrap();

    let second = Service::new(config(&dir));

    assert!(second.is_err());
}

const SLOW: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) \
                    SELECT count(*) FROM c";

#[tokio::test]
async fn deadline_interrupts_a_statement() {
    let dir = TempDir::new();
    let service = Service::new(Config {
        timeout: Duration::from_millis(200),
        ..config(&dir)
    })
    .unwrap();

    let started = std::time::Instant::now();
    let (kind, message, _) = failure(query(&service, SLOW, vec![]).await);

    assert_eq!(kind, D1ErrorKind::Interrupted, "{message}");
    assert!(started.elapsed() < Duration::from_secs(2));

    let after = rows(query(&service, "SELECT 1", vec![]).await);

    assert_eq!(json(&after.rows), "[[1]]");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_queue_refuses_calls() {
    let dir = TempDir::new();
    let service = Arc::new(
        Service::new(Config {
            timeout: Duration::from_millis(500),
            max_queue: 1,
            ..config(&dir)
        })
        .unwrap(),
    );

    let slow = tokio::spawn({
        let service = service.clone();

        async move { query(&service, SLOW, vec![]).await }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (kind, _, _) = failure(query(&service, "SELECT 1", vec![]).await);

    assert_eq!(kind, D1ErrorKind::Overloaded);

    slow.await.unwrap();
}

#[tokio::test]
async fn a_slow_database_does_not_block_another() {
    let dir = TempDir::new();
    let service = Arc::new(
        Service::new(Config {
            timeout: Duration::from_secs(2),
            ..config(&dir)
        })
        .unwrap(),
    );

    let slow = tokio::spawn({
        let service = service.clone();

        async move { query(&service, SLOW, vec![]).await }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let started = std::time::Instant::now();
    let op = D1Op::Exec {
        sql: "SELECT 1".into(),
    };

    assert!(matches!(
        service.execute(OTHER_DB, op).await,
        D1Result::Exec { .. }
    ));
    assert!(started.elapsed() < Duration::from_millis(500));

    slow.await.unwrap();
}

#[tokio::test]
async fn database_size_is_bounded() {
    let dir = TempDir::new();
    let service = Service::new(Config {
        limits: Limits {
            database_bytes: 64 * 1024,
            ..Limits::default()
        },
        ..config(&dir)
    })
    .unwrap();

    exec(&service, "CREATE TABLE t (a BLOB)").await;

    let (kind, message, _) =
        failure(query(&service, "INSERT INTO t VALUES (zeroblob(200000))", vec![]).await);

    assert_eq!(kind, D1ErrorKind::Overloaded, "{message}");
}
