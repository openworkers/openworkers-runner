//! WASM runtime tests
//!
//! Minimal tests for the WebAssembly runtime.

use openworkers_core::{TerminationReason, WorkerCode};
use openworkers_runner::store::{
    Binding, CodeType, DatabaseConfig, DatabaseProvider, KvConfig, StorageConfig,
    WorkerBindingConfig, WorkerWithBindings,
};
use openworkers_runner::worker::prepare_script;
use std::collections::HashMap;

/// Helper to create a WorkerWithBindings for testing
fn create_test_worker(code: Vec<u8>, code_type: CodeType) -> WorkerWithBindings {
    WorkerWithBindings {
        id: "test-worker".to_string(),
        name: Some("test".to_string()),
        user_id: "test-user".to_string(),
        code,
        code_type,
        version: 1,
        env: HashMap::new(),
        bindings: vec![],
        env_updated_at: None,
    }
}

/// A build carrying only the wasm backend runs components, so a JavaScript
/// worker has to be refused
#[cfg(not(feature = "_js"))]
#[test]
fn test_prepare_script_rejects_javascript() {
    let worker = create_test_worker(
        b"export default { fetch() { return new Response('hello'); } }".to_vec(),
        CodeType::Javascript,
    );

    match prepare_script(&worker) {
        Err(TerminationReason::InitializationError(msg)) => {
            assert!(msg.contains("cannot run JavaScript"));
        }
        _ => panic!("Expected InitializationError"),
    }
}

#[cfg(not(feature = "_js"))]
#[test]
fn test_prepare_script_rejects_typescript() {
    let worker = create_test_worker(
        b"export default { fetch(): Response { return new Response('hello'); } }".to_vec(),
        CodeType::Typescript,
    );

    match prepare_script(&worker) {
        Err(TerminationReason::InitializationError(msg)) => {
            assert!(msg.contains("cannot run JavaScript"));
        }
        _ => panic!("Expected InitializationError"),
    }
}

/// Test that WASM code is accepted
#[test]
fn test_prepare_script_accepts_wasm() {
    // Minimal valid WASM module (empty module)
    let wasm_bytes = wat::parse_str("(module)").expect("Failed to parse WAT");

    let worker = create_test_worker(wasm_bytes, CodeType::Wasm);

    let script = prepare_script(&worker).expect("wasm should prepare");

    assert!(matches!(script.code, WorkerCode::WebAssembly(_)));
}

fn empty_module() -> WorkerWithBindings {
    create_test_worker(
        wat::parse_str("(module)").expect("Failed to parse WAT"),
        CodeType::Wasm,
    )
}

fn storage_config() -> StorageConfig {
    StorageConfig {
        id: "storage-id".to_string(),
        bucket: "bucket".to_string(),
        prefix: None,
        access_key_id: "key".to_string(),
        secret_access_key: "secret".to_string(),
        endpoint: "http://localhost:9000".to_string(),
        region: None,
        public_url: None,
    }
}

/// The three binding types the openworkers:bindings WIT package covers
#[test]
fn test_prepare_script_accepts_kv_database_and_storage() {
    let mut worker = empty_module();

    worker.bindings = vec![
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
            config: storage_config(),
        },
    ];

    let script = prepare_script(&worker).expect("wasm serves these binding types");

    assert_eq!(script.bindings.len(), 3);
}

/// Nothing in the WIT serves assets or worker bindings, and the error has to
/// name the types so the operator knows which bindings to drop
#[test]
fn test_prepare_script_refuses_assets_and_worker_bindings() {
    let mut worker = empty_module();

    worker.bindings = vec![
        Binding::Assets {
            key: "ASSETS".to_string(),
            config: storage_config(),
        },
        Binding::Worker {
            key: "SERVICE".to_string(),
            config: WorkerBindingConfig {
                id: "worker-id".to_string(),
                name: Some("other".to_string()),
            },
        },
    ];

    match prepare_script(&worker) {
        Err(TerminationReason::InitializationError(msg)) => {
            assert_eq!(
                msg,
                "the wasm backend does not implement assets/worker bindings, \
                 declared as ASSETS, SERVICE"
            );
        }
        other => panic!("Expected InitializationError, got {other:?}"),
    }
}
