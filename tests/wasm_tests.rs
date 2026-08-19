//! WASM runtime tests
//!
//! Minimal tests for the WebAssembly runtime.

use openworkers_core::{TerminationReason, WorkerCode};
use openworkers_runner::store::{CodeType, WorkerWithBindings};
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

/// The wasm backend runs components, so a JavaScript worker has to be refused
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
