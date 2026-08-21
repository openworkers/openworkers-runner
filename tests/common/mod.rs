//! Fixtures shared by the tests that run a wasm guest.

use openworkers_core::DatabaseOp;
use openworkers_core::DatabaseResult;
use openworkers_core::KvOp;
use openworkers_core::KvResult;
use openworkers_core::OpFuture;
use openworkers_core::OperationsHandler;
use openworkers_core::SqlParam;
use openworkers_core::StorageOp;
use openworkers_core::StorageResult;
use openworkers_runner::store::Binding;
use openworkers_runner::store::CodeType;
use openworkers_runner::store::DatabaseConfig;
use openworkers_runner::store::DatabaseProvider;
use openworkers_runner::store::KvConfig;
use openworkers_runner::store::StorageConfig;
use openworkers_runner::store::WorkerWithBindings;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/bindings-worker"
);

/// What the guest answers a GET with, once every binding replied
pub const PROBE_RESPONSE: &str = r#"kv={"count":1} db={"name":"widget"} storage=round trip"#;

/// The guest component, built on first use and shared by the tests in this
/// binary; two cargo builds of the fixture would only queue on its lock
pub fn probe_component() -> Vec<u8> {
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

/// A worker row for the probe component, with the three bindings it reaches
pub fn probe_worker(id: &str, code: Vec<u8>) -> WorkerWithBindings {
    WorkerWithBindings {
        id: id.to_string(),
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
pub struct RecordingOps {
    pub kv: Mutex<Vec<(String, KvOp)>>,
    pub queries: Mutex<Vec<(String, String, Vec<SqlParam>)>>,
    pub storage: Mutex<Vec<(String, StorageOp)>>,
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
