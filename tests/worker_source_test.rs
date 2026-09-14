//! The store decides once whether a worker's code comes from the cache.
//!
//! A cached entry stands in for the deployment's bytes, which then need not be
//! fetched at all; anything but a JavaScript worker keeps its bytes.

use openworkers_runner::code_cache;
use openworkers_runner::store::CodeType;
use openworkers_runner::store::WorkerData;
use openworkers_runner::store::WorkerSource;
use openworkers_runner::store::WorkerWithBindings;

fn deployment(id: &str, code_type: CodeType) -> WorkerData {
    WorkerData {
        id: id.to_string(),
        name: None,
        user_id: "user".to_string(),
        env: None,
        code: b"source".to_vec(),
        code_type,
        version: 7,
    }
}

#[test]
fn a_cold_worker_keeps_its_bytes() {
    let worker = WorkerWithBindings::from(deployment("source-cold", CodeType::Javascript));

    assert!(matches!(&worker.code, WorkerSource::Bytes(b) if b == b"source"));
}

#[test]
fn a_cached_version_stands_in_for_the_bytes() {
    code_cache::put("source-warm", 7, b"snapshot");

    let worker = WorkerWithBindings::from(deployment("source-warm", CodeType::Javascript));

    assert!(matches!(&worker.code, WorkerSource::Cached(b) if b == b"snapshot"));
}

#[test]
fn another_version_of_the_same_worker_is_cold() {
    code_cache::put("source-stale", 6, b"old snapshot");

    let worker = WorkerWithBindings::from(deployment("source-stale", CodeType::Javascript));

    assert!(matches!(&worker.code, WorkerSource::Bytes(_)));
}

#[test]
fn a_wasm_worker_keeps_its_bytes_whatever_the_cache_holds() {
    code_cache::put("source-wasm", 7, b"not for wasm");

    let worker = WorkerWithBindings::from(deployment("source-wasm", CodeType::Wasm));

    assert!(matches!(&worker.code, WorkerSource::Bytes(b) if b == b"source"));
}
