//! End-to-end tests for the wasm backend.
//!
//! A wasip2 component goes through the runner's own dispatch path
//! (`prepare_script` + `create_worker` + a fetch event) with a recording
//! operations handler standing in for `RunnerOperations`, so the whole chain
//! from a declared binding to a platform operation is covered.

use openworkers_core::{
    DatabaseOp, DatabaseResult, Event, HttpMethod, HttpRequest, KvOp, KvResult, OpFuture,
    OperationsHandler, RequestBody, RuntimeLimits, SqlParam, SqlPrimitive, StorageOp,
    StorageResult, WorkerCode,
};
use openworkers_runner::store::{
    Binding, CodeType, DatabaseConfig, DatabaseProvider, KvConfig, StorageConfig,
    WorkerWithBindings,
};
use openworkers_runner::task_executor::TaskExecutionConfig;
use openworkers_runner::worker::{
    create_cached_worker, create_worker, prepare_script, prepare_worker,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/bindings-worker"
);

/// The guest component, built on first use and shared by the tests in this
/// binary; two cargo builds of the fixture would only queue on its lock
fn probe_component() -> Vec<u8> {
    static COMPONENT: OnceLock<Vec<u8>> = OnceLock::new();

    COMPONENT.get_or_init(build_probe_component).clone()
}

/// Build the guest component; it is a fixture, so no artifact is checked in
fn build_probe_component() -> Vec<u8> {
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .args(["--manifest-path", &format!("{FIXTURE}/Cargo.toml")])
        .args(["--target-dir", &format!("{FIXTURE}/target")])
        // Inheriting the outer build's target dir would deadlock on its lock
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("cargo should run");

    assert!(
        status.success(),
        "building {FIXTURE} failed; the wasm32-wasip2 target has to be installed"
    );

    let path = format!("{FIXTURE}/target/wasm32-wasip2/release/bindings_worker.wasm");

    std::fs::read(&path).unwrap_or_else(|e| panic!("could not read {path}: {e}"))
}

fn probe_worker(code: Vec<u8>) -> WorkerWithBindings {
    WorkerWithBindings {
        id: "test-worker".to_string(),
        name: Some("test".to_string()),
        user_id: "test-user".to_string(),
        code,
        code_type: CodeType::Wasm,
        version: 1,
        env: HashMap::new(),
        bindings: vec![
            Binding::Kv {
                key: "CACHE".to_string(),
                config: KvConfig {
                    id: "kv-id".to_string(),
                    name: "cache".to_string(),
                },
            },
            Binding::Database {
                key: "DB".to_string(),
                config: DatabaseConfig {
                    id: "database-id".to_string(),
                    name: "db".to_string(),
                    provider: DatabaseProvider::Platform,
                    connection_string: None,
                    schema_name: Some("tenant".to_string()),
                    max_rows: 100,
                    timeout_seconds: 5,
                },
            },
            Binding::Storage {
                key: "BUCKET".to_string(),
                config: StorageConfig {
                    id: "storage-id".to_string(),
                    bucket: "bucket".to_string(),
                    prefix: None,
                    access_key_id: "key".to_string(),
                    secret_access_key: "secret".to_string(),
                    endpoint: "http://localhost:9000".to_string(),
                    region: None,
                    public_url: None,
                },
            },
        ],
        env_updated_at: None,
    }
}

/// Stands in for `RunnerOperations`: records what each binding was asked for,
/// and keeps enough state to answer a read that follows a write
#[derive(Default)]
struct RecordingOps {
    kv: Mutex<Vec<(String, KvOp)>>,
    queries: Mutex<Vec<(String, String, Vec<SqlParam>)>>,
    storage: Mutex<Vec<(String, StorageOp)>>,
    values: Mutex<HashMap<String, serde_json::Value>>,
    objects: Mutex<HashMap<String, Vec<u8>>>,
}

impl OperationsHandler for RecordingOps {
    fn handle_binding_kv(&self, binding: &str, op: KvOp) -> OpFuture<'_, KvResult> {
        self.kv
            .lock()
            .unwrap()
            .push((binding.to_string(), op.clone()));

        let mut values = self.values.lock().unwrap();

        let result = match op {
            KvOp::Get { key } => KvResult::Value(values.get(&key).cloned()),
            KvOp::Put { key, value, .. } => {
                values.insert(key, value);
                KvResult::Ok
            }
            other => KvResult::Error(format!("unexpected kv op: {other:?}")),
        };

        Box::pin(async move { result })
    }

    fn handle_binding_database(
        &self,
        binding: &str,
        op: DatabaseOp,
    ) -> OpFuture<'_, DatabaseResult> {
        let DatabaseOp::Query { sql, params } = op;

        self.queries
            .lock()
            .unwrap()
            .push((binding.to_string(), sql, params));

        Box::pin(async move { DatabaseResult::Rows(r#"[{"name":"widget"}]"#.to_string()) })
    }

    fn handle_binding_storage(&self, binding: &str, op: StorageOp) -> OpFuture<'_, StorageResult> {
        self.storage
            .lock()
            .unwrap()
            .push((binding.to_string(), op.clone()));

        let mut objects = self.objects.lock().unwrap();

        let result = match op {
            StorageOp::Get { key } => StorageResult::Body(objects.get(&key).cloned()),
            StorageOp::Put { key, body } => {
                objects.insert(key, body);
                StorageResult::Body(None)
            }
            other => StorageResult::Error(format!("unexpected storage op: {other:?}")),
        };

        Box::pin(async move { result })
    }
}

#[tokio::test]
async fn a_wasm_worker_reaches_kv_database_and_storage() {
    let data = probe_worker(probe_component());

    let script = prepare_script(&data).expect("the wasm backend serves these binding types");

    assert_eq!(script.bindings.len(), 3);

    let ops = Arc::new(RecordingOps::default());

    let mut worker = create_worker(script, TaskExecutionConfig::default_limits(), ops.clone())
        .await
        .expect("worker should initialize");

    let (event, rx) = Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    });

    worker.exec(event).await.expect("fetch should run");

    let response = rx.await.expect("worker should respond");
    let body = response
        .body
        .collect()
        .await
        .expect("body stream failed")
        .expect("response has a body");
    let body = String::from_utf8_lossy(&body);

    assert_eq!(response.status, 200, "guest reported: {body}");
    assert_eq!(
        body,
        r#"kv={"count":1} db={"name":"widget"} storage=round trip"#
    );

    let kv = ops.kv.lock().unwrap();

    assert_eq!(kv.len(), 2);
    assert_eq!(kv[0].0, "CACHE");
    assert!(
        matches!(&kv[0].1, KvOp::Put { key, expires_in, .. } if key == "visits" && *expires_in == Some(60))
    );
    assert!(matches!(&kv[1].1, KvOp::Get { key } if key == "visits"));

    let queries = ops.queries.lock().unwrap();

    assert_eq!(queries.len(), 1);

    let (binding, sql, params) = &queries[0];

    assert_eq!(binding, "DB");
    assert_eq!(
        sql,
        "SELECT name FROM items WHERE id = $1 AND active = $2 AND name = $3"
    );
    assert!(matches!(
        params[0],
        SqlParam::Primitive(SqlPrimitive::Int(7))
    ));
    assert!(matches!(
        params[1],
        SqlParam::Primitive(SqlPrimitive::Bool(true))
    ));
    assert!(matches!(
        &params[2],
        SqlParam::Primitive(SqlPrimitive::String(s)) if s == "widget"
    ));

    let storage = ops.storage.lock().unwrap();

    assert_eq!(storage.len(), 2);
    assert_eq!(storage[0].0, "BUCKET");
    assert!(
        matches!(&storage[0].1, StorageOp::Put { key, body } if key == "hello.txt" && body == b"round trip")
    );
    assert!(matches!(&storage[1].1, StorageOp::Get { key } if key == "hello.txt"));
}

/// A component is compiled once per worker version: the second cold start
/// loads what the first one precompiled instead of running Cranelift again.
#[tokio::test]
async fn a_second_cold_start_loads_the_precompiled_component() {
    let mut data = probe_worker(probe_component());

    // Its own id, so the binding test above cannot seed this cache entry
    data.id = "component-cache-worker".to_string();

    let limits = TaskExecutionConfig::default_limits();
    let hits_before = openworkers_runner::wasm_cache::hits();

    let first = serve_one_request(&data, limits.clone()).await;

    let prepared = prepare_worker(&data, &limits).expect("preparing should work");

    assert!(
        prepared.precompiled.is_some(),
        "the first cold start should have cached its component"
    );

    assert!(
        matches!(&prepared.script.code, WorkerCode::WebAssembly(bytes) if bytes.is_empty()),
        "a cache hit should not carry a copy of the guest bytes"
    );

    let second = serve_one_request(&data, limits.clone()).await;

    assert_eq!(
        openworkers_runner::wasm_cache::hits(),
        hits_before + 1,
        "the second cold start should have come from the cache"
    );

    assert_eq!(first, second);
}

/// One cold start, from the worker row to the response body
async fn serve_one_request(data: &WorkerWithBindings, limits: RuntimeLimits) -> String {
    let prepared = prepare_worker(data, &limits).expect("the wasm backend serves these bindings");
    let ops = Arc::new(RecordingOps::default());

    let mut worker = create_cached_worker(prepared, limits, ops, &data.id, data.version)
        .await
        .expect("worker should initialize");

    let (event, rx) = Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    });

    worker.exec(event).await.expect("fetch should run");

    let response = rx.await.expect("worker should respond");
    let body = response
        .body
        .collect()
        .await
        .expect("body stream failed")
        .expect("response has a body");
    let body = String::from_utf8_lossy(&body).into_owned();

    assert_eq!(response.status, 200, "guest reported: {body}");

    body
}
