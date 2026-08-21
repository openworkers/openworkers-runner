//! End-to-end tests for the wasm backend.
//!
//! A wasip2 component goes through the runner's own dispatch path
//! (`prepare_script` + `create_worker` + a fetch event) with a recording
//! operations handler standing in for `RunnerOperations`, so the whole chain
//! from a declared binding to a platform operation is covered.

mod common;

use common::PROBE_RESPONSE;
use common::RecordingOps;
use common::probe_component;
use common::probe_worker;
use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::KvOp;
use openworkers_core::RequestBody;
use openworkers_core::RuntimeLimits;
use openworkers_core::SqlParam;
use openworkers_core::SqlPrimitive;
use openworkers_core::StorageOp;
use openworkers_core::WorkerCode;
use openworkers_runner::store::WorkerWithBindings;
use openworkers_runner::task_executor::TaskExecutionConfig;
use openworkers_runner::worker::create_cached_worker;
use openworkers_runner::worker::create_worker;
use openworkers_runner::worker::prepare_script;
use openworkers_runner::worker::prepare_worker;
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test]
async fn a_wasm_worker_reaches_kv_database_and_storage() {
    let data = probe_worker("test-worker", probe_component());

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
    assert_eq!(body, PROBE_RESPONSE);

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
    // Its own id, so the binding test above cannot seed this cache entry
    let data = probe_worker("component-cache-worker", probe_component());

    let limits = TaskExecutionConfig::default_limits();
    let hits_before = openworkers_runner::wasm_cache::hits();

    let first = serve_one_request(&data, limits.clone()).await;

    let prepared = prepare_worker(&data, &limits).expect("preparing should work");

    assert!(
        prepared.prepared.is_some(),
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
