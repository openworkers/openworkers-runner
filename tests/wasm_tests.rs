//! WASM runtime tests
//!
//! Minimal tests for the WebAssembly runtime.

#![cfg(feature = "wasm")]

use openworkers_core::{TerminationReason, WorkerCode};
use openworkers_runner::store::{CodeType, WorkerWithBindings};
use openworkers_runner::worker::prepare_script;
use std::collections::HashMap;

/// Helper to create a WorkerWithBindings for testing
fn create_test_worker(code: Vec<u8>, code_type: CodeType) -> WorkerWithBindings {
    WorkerWithBindings {
        id: "test-worker".to_string(),
        name: "test".to_string(),
        code,
        code_type,
        checksum: 0,
        env: HashMap::new(),
        bindings: vec![],
    }
}

/// Test that JS is rejected when WASM-only (no v8 feature)
#[cfg(not(feature = "v8"))]
#[test]
fn test_prepare_script_rejects_javascript() {
    let worker = create_test_worker(
        b"export default { fetch() { return new Response('hello'); } }".to_vec(),
        CodeType::Javascript,
    );

    let result = prepare_script(&worker);
    assert!(result.is_err());

    match result {
        Err(TerminationReason::InitializationError(msg)) => {
            assert!(msg.contains("JavaScript runtime not available"));
        }
        _ => panic!("Expected InitializationError"),
    }
}

/// Test that TS is rejected when WASM-only (no v8 feature)
#[cfg(not(feature = "v8"))]
#[test]
fn test_prepare_script_rejects_typescript() {
    let worker = create_test_worker(
        b"export default { fetch(): Response { return new Response('hello'); } }".to_vec(),
        CodeType::Typescript,
    );

    let result = prepare_script(&worker);
    assert!(result.is_err());

    match result {
        Err(TerminationReason::InitializationError(msg)) => {
            assert!(msg.contains("JavaScript runtime not available"));
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

    let result = prepare_script(&worker);
    assert!(result.is_ok());

    let script = result.unwrap();
    assert!(matches!(script.code, WorkerCode::WebAssembly(_)));
}

/// Test that both JS and WASM work in dual-runtime mode
#[cfg(feature = "v8")]
#[test]
fn test_dual_runtime_accepts_both() {
    // WASM
    let wasm_bytes = wat::parse_str("(module)").expect("Failed to parse WAT");
    let wasm_worker = create_test_worker(wasm_bytes, CodeType::Wasm);
    assert!(prepare_script(&wasm_worker).is_ok());

    // JavaScript
    let js_worker = create_test_worker(
        b"export default { fetch() { return new Response('hello'); } }".to_vec(),
        CodeType::Javascript,
    );
    assert!(prepare_script(&js_worker).is_ok());
}
