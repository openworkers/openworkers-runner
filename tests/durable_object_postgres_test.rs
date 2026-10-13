//! Durable Object namespaces and snapshots in the platform database.
//!
//! These tests need a database with the openworkers-cli migrations:
//! `TEST_DATABASE_URL=postgres://... cargo test --features v8 --test durable_object_postgres_test -- --ignored`

use openworkers_runner::Binding;
use openworkers_runner::d1::archive::{Index, PgIndex, Snapshot};
use openworkers_runner::store::{WorkerIdentifier, get_worker_meta};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

async fn database() -> PgPool {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL is not set");

    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap()
}

async fn new_user(db: &PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO users (username) VALUES ($1) RETURNING id")
        .bind(format!("test-{}", Uuid::new_v4().simple()))
        .fetch_one(db)
        .await
        .unwrap()
}

/// A worker of `user` whose environment binds each of `namespaces` under
/// the names `DO0`, `DO1`...
async fn new_worker(db: &PgPool, user: Uuid, namespaces: &[Uuid]) -> Uuid {
    let environment: Uuid =
        sqlx::query_scalar("INSERT INTO environments (name, user_id) VALUES ($1, $2) RETURNING id")
            .bind(format!("env-{}", Uuid::new_v4().simple()))
            .bind(user)
            .fetch_one(db)
            .await
            .unwrap();

    for (index, namespace) in namespaces.iter().enumerate() {
        sqlx::query(
            "INSERT INTO environment_values (environment_id, user_id, key, value, type) \
             VALUES ($1, $2, $3, $4, 'durable_object')",
        )
        .bind(environment)
        .bind(user)
        .bind(format!("DO{index}"))
        .bind(namespace.to_string())
        .execute(db)
        .await
        .unwrap();
    }

    let worker: Uuid = sqlx::query_scalar(
        "INSERT INTO workers (user_id, environment_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(user)
    .bind(environment)
    .fetch_one(db)
    .await
    .unwrap();

    // The runner serves a worker that has a deployment
    sqlx::query(
        "INSERT INTO worker_deployments (worker_id, version, hash, code_type, code) \
         VALUES ($1, 1, $2, 'javascript', 'export class Counter {}')",
    )
    .bind(worker)
    .bind("0".repeat(64))
    .execute(db)
    .await
    .unwrap();

    sqlx::query("UPDATE workers SET current_version = 1 WHERE id = $1")
        .bind(worker)
        .execute(db)
        .await
        .unwrap();

    worker
}

async fn new_namespace(db: &PgPool, user: Uuid, worker: Uuid, class_name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO durable_object_namespaces (user_id, worker_id, class_name, name) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(user)
    .bind(worker)
    .bind(class_name)
    .bind(format!("ns-{}", Uuid::new_v4().simple()))
    .fetch_one(db)
    .await
    .unwrap()
}

/// Removes a test account; its environments do not go with it by cascade.
async fn remove_user(db: &PgPool, user: Uuid) {
    for sql in [
        "DELETE FROM workers WHERE user_id = $1",
        "DELETE FROM environments WHERE user_id = $1",
        "DELETE FROM users WHERE id = $1",
    ] {
        sqlx::query(sql).bind(user).execute(db).await.unwrap();
    }
}

/// The namespaces a worker reads through its bindings, by binding name.
async fn bound(db: &PgPool, worker: Uuid) -> Vec<(String, String, String)> {
    let mut conn = db.acquire().await.unwrap();
    let meta = get_worker_meta(&mut conn, WorkerIdentifier::Id(worker.to_string()))
        .await
        .unwrap();

    let mut bound: Vec<(String, String, String)> = meta
        .bindings
        .into_iter()
        .filter_map(|binding| match binding {
            Binding::DurableObject { key, namespace } => {
                assert_eq!(namespace.user_id, meta.user_id);
                Some((key, namespace.id, namespace.class_name))
            }
            _ => None,
        })
        .collect();

    bound.sort();
    bound
}

#[tokio::test]
#[ignore]
async fn a_worker_reads_the_namespaces_its_account_owns() {
    let db = database().await;

    let user = new_user(&db).await;
    let exporter = new_worker(&db, user, &[]).await;
    let counters = new_namespace(&db, user, exporter, "Counter").await;
    let rooms = new_namespace(&db, user, exporter, "Room").await;

    let stranger = new_user(&db).await;
    let theirs = new_worker(&db, stranger, &[]).await;
    let foreign = new_namespace(&db, stranger, theirs, "Counter").await;

    let worker = new_worker(&db, user, &[counters, rooms, foreign]).await;

    assert_eq!(
        bound(&db, worker).await,
        [
            (
                "DO0".to_string(),
                counters.to_string(),
                "Counter".to_string()
            ),
            ("DO1".to_string(), rooms.to_string(), "Room".to_string()),
        ]
    );

    // A deleted namespace, and one whose worker is gone, bind nothing
    sqlx::query("UPDATE durable_object_namespaces SET deleted_at = now() WHERE id = $1")
        .bind(rooms)
        .execute(&db)
        .await
        .unwrap();

    let other = new_worker(&db, user, &[]).await;
    let orphan = new_namespace(&db, user, other, "Orphan").await;
    sqlx::query("DELETE FROM workers WHERE id = $1")
        .bind(other)
        .execute(&db)
        .await
        .unwrap();

    let worker = new_worker(&db, user, &[counters, rooms, orphan]).await;

    assert_eq!(
        bound(&db, worker).await,
        [(
            "DO0".to_string(),
            counters.to_string(),
            "Counter".to_string()
        )]
    );

    remove_user(&db, user).await;
    remove_user(&db, stranger).await;
}

#[tokio::test]
#[ignore]
async fn object_snapshots_have_an_index_of_their_own() {
    let db = database().await;

    let user = new_user(&db).await;
    let worker = new_worker(&db, user, &[]).await;
    let namespace = new_namespace(&db, user, worker, "Counter").await;
    let object = "ef".repeat(32);
    let key = format!("objects/{namespace}/{object}");

    let index = PgIndex::start(db.clone()).await.unwrap();
    let snapshot = |seq| Snapshot {
        database_id: key.clone(),
        seq,
        epoch: index.epoch(),
        key: format!("do/{namespace}/{object}/{}/{seq}.sqlite.zst", index.epoch()),
        checksum: "0".repeat(64),
        size: 10,
    };

    index.record(snapshot(1)).await.unwrap();
    index.record(snapshot(2)).await.unwrap();

    let listed = index.list(key.clone()).await.unwrap();
    assert_eq!(listed, [snapshot(2), snapshot(1)]);

    index.forget(key.clone(), 1).await.unwrap();
    assert_eq!(index.list(key.clone()).await.unwrap(), [snapshot(2)]);

    // A forgotten snapshot goes to the purge queue, for its object
    let queued: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM durable_object_purge WHERE namespace_id = $1 AND key IS NOT NULL",
    )
    .bind(namespace)
    .fetch_all(&db)
    .await
    .unwrap();

    assert_eq!(queued, [snapshot(1).key]);

    remove_user(&db, user).await;
    sqlx::query("DELETE FROM durable_object_purge WHERE namespace_id = $1")
        .bind(namespace)
        .execute(&db)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn an_old_deployment_or_a_deleted_worker_is_outdated() {
    let db = database().await;

    let user = new_user(&db).await;
    let worker = new_worker(&db, user, &[]).await.to_string();
    let gone = Uuid::new_v4().to_string();

    let running = [(worker.clone(), 1), (worker.clone(), 0), (gone.clone(), 1)];
    let outdated = openworkers_runner::durable::outdated(&db, &running)
        .await
        .unwrap();

    assert_eq!(outdated, [(worker.clone(), 0), (gone, 1)]);

    remove_user(&db, user).await;
}

fn data_dir() -> openworkers_runner::d1::Service {
    let dir = std::env::temp_dir().join(format!("do-alarm-test-{}", Uuid::new_v4()));

    openworkers_runner::d1::Service::new(openworkers_runner::d1::Config {
        data_dir: dir,
        limits: Default::default(),
        timeout: std::time::Duration::from_secs(5),
        max_queue: 64,
        max_open: 16,
        idle: std::time::Duration::from_secs(300),
        min_free_bytes: 0,
        namespace_bytes: u64::MAX,
    })
    .unwrap()
}

async fn indexed(db: &PgPool, namespace: Uuid, object: &str) -> Option<(i64, i32, bool)> {
    sqlx::query_as(
        "SELECT (extract(epoch FROM scheduled_at) * 1000)::bigint, retry_count, delivering \
         FROM durable_object_alarms WHERE namespace_id = $1 AND object_id = $2",
    )
    .bind(namespace)
    .bind(object)
    .fetch_optional(db)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn an_alarm_goes_to_the_object_file_and_to_the_due_index() {
    use openworkers_core::ObjectStorageResult;
    use openworkers_runner::durable::alarms;

    let db = database().await;
    let service = data_dir();

    let user = new_user(&db).await;
    let worker = new_worker(&db, user, &[]).await;
    let namespace = new_namespace(&db, user, worker, "Counter").await;
    let ns = namespace.to_string();
    let object = "cd".repeat(32);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let later = now + 60_000;
    assert_eq!(
        alarms::set(&service, &db, &ns, &object, Some(later)).await,
        ObjectStorageResult::Alarm(Some(later))
    );
    assert_eq!(
        indexed(&db, namespace, &object).await,
        Some((later, 0, false))
    );

    let mine = |due: &[alarms::Due]| due.iter().any(|due| due.object_id == object);
    assert!(!mine(&alarms::claim(&db, 1000).await.unwrap()));

    // A due alarm is claimed once, and stays under way through a setAlarm
    let past = now - 1000;
    alarms::set(&service, &db, &ns, &object, Some(past)).await;
    assert!(mine(&alarms::claim(&db, 1000).await.unwrap()));
    assert!(!mine(&alarms::claim(&db, 1000).await.unwrap()));

    alarms::set(&service, &db, &ns, &object, Some(later)).await;
    assert_eq!(
        indexed(&db, namespace, &object).await,
        Some((later, 0, true))
    );

    alarms::set(&service, &db, &ns, &object, None).await;
    assert_eq!(indexed(&db, namespace, &object).await, None);

    remove_user(&db, user).await;
}

#[cfg(feature = "v8")]
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn an_alarm_the_account_limit_refuses_is_not_a_failed_run() {
    use openworkers_runner::durable::id::IdKey;
    use openworkers_runner::durable::{self, NamespaceConfig, ObjectSource, alarms};

    let db = database().await;
    openworkers_runner::d1::install(data_dir());
    let service = openworkers_runner::d1::service().unwrap();

    let user = new_user(&db).await;
    let worker = new_worker(&db, user, &[]).await;
    let namespace = new_namespace(&db, user, worker, "Counter").await;
    let ns = namespace.to_string();
    let object = "ef".repeat(32);

    // The account runs as many objects as it may, in another namespace
    let busy = NamespaceConfig {
        id: Uuid::new_v4().to_string(),
        user_id: user.to_string(),
        worker_id: worker.to_string(),
        class_name: "Counter".to_string(),
        key: IdKey([5; 32]),
    };
    let started = futures::future::join_all((0..durable::MAX_RESIDENT_PER_ACCOUNT).map(|n| {
        let load = |_| async {
            let code = openworkers_transform::parse_worker_code(
                b"export class Counter { fetch() { return new Response('up'); } }",
                openworkers_transform::CodeLanguage::JavaScript,
            )
            .unwrap();

            Ok(ObjectSource {
                script: openworkers_core::Script::new(code),
                ops: std::sync::Arc::new(openworkers_runner::ops::RunnerOperations::new()),
                version: 1,
                limits: openworkers_runner::task_executor::TaskExecutionConfig::default_limits(),
                idle: std::time::Duration::from_secs(60),
                drain: durable::DRAIN,
            })
        };
        let request = openworkers_core::HttpRequest {
            method: openworkers_core::HttpMethod::Get,
            url: "http://object/".to_string(),
            headers: Default::default(),
            body: openworkers_core::RequestBody::None,
        };

        durable::send(&busy, n.to_string(), 0, request, load)
    }))
    .await;

    for answer in started {
        answer.unwrap();
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    alarms::set(service, &db, &ns, &object, Some(now - 1000)).await;

    let (log_sink, _logs) = openworkers_runner::log::LogSink::new();
    let due = alarms::Due {
        namespace_id: ns.clone(),
        object_id: object.clone(),
        retry_count: 0,
    };
    alarms::deliver(db.clone(), db.clone(), log_sink, due).await;

    // The handler did not run: the alarm runs again later, with no failed run
    let (at, retry_count, delivering) = indexed(&db, namespace, &object).await.unwrap();
    assert_eq!((retry_count, delivering), (0, false));
    assert!(at > now);

    durable::stop_namespace(&busy.id);
    remove_user(&db, user).await;
}

#[cfg(feature = "v8")]
#[tokio::test]
#[ignore]
async fn the_worker_logs_tell_why_an_object_did_not_load() {
    use openworkers_runner::durable::id::IdKey;
    use openworkers_runner::durable::{self, NamespaceConfig};
    use openworkers_runner::log::{LogSender, LogSink};

    let db = database().await;
    let user = new_user(&db).await;
    let worker = new_worker(&db, user, &[]).await;

    sqlx::query("UPDATE worker_deployments SET code_type = 'wasm' WHERE worker_id = $1")
        .bind(worker)
        .execute(&db)
        .await
        .unwrap();

    let namespace = NamespaceConfig {
        id: Uuid::new_v4().to_string(),
        user_id: user.to_string(),
        worker_id: worker.to_string(),
        class_name: "Counter".to_string(),
        key: IdKey([6; 32]),
    };
    let (sink, _store) = LogSink::new();
    let mut logs = sink.subscribe();

    let error = durable::load(
        namespace,
        "a".to_string(),
        Default::default(),
        Some(db.clone()),
        Some(LogSender::new("caller".to_string(), sink)),
    )
    .await
    .err()
    .unwrap();

    let line = logs.try_recv().unwrap();
    assert_eq!(line.worker_id, worker.to_string());
    assert_eq!(
        line.message,
        format!("Durable Object Counter a: not loaded: {error}")
    );

    remove_user(&db, user).await;
}
