//! One process, both backends.
//!
//! A JavaScript worker and a wasm component are served by the same runner, each
//! through `prepare_worker` + `create_cached_worker`, so the dispatch that picks
//! a backend from the worker's code is covered end to end.

mod common;

use common::PROBE_RESPONSE;
use common::RecordingOps;
use common::probe_component;
use common::probe_worker;
use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::OperationsHandle;
use openworkers_core::RequestBody;
use openworkers_runner::ops::RunnerOperations;
use openworkers_runner::store::CodeType;
use openworkers_runner::store::WorkerWithBindings;
use openworkers_runner::task_executor::TaskExecutionConfig;
use openworkers_runner::worker::create_cached_worker;
use openworkers_runner::worker::prepare_worker;
use std::collections::HashMap;
use std::sync::Arc;

fn js_worker() -> WorkerWithBindings {
    WorkerWithBindings {
        id: "cohabitation-js".to_string(),
        name: Some("js".to_string()),
        user_id: "test-user".to_string(),
        code: b"addEventListener('fetch', (event) => event.respondWith(new Response('js')));"
            .to_vec(),
        code_type: CodeType::Javascript,
        version: 1,
        env: HashMap::new(),
        bindings: vec![],
        env_updated_at: None,
    }
}

#[tokio::test]
async fn a_javascript_worker_and_a_component_are_served_by_the_same_process() {
    let js = js_worker();
    let wasm = probe_worker("cohabitation-wasm", probe_component());

    // The JavaScript backend holds thread-local state, so its worker stays on
    // one thread for its whole life
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
            let js_body = serve_one_request(&js, Arc::new(RunnerOperations::new())).await;

            assert_eq!(js_body, "js");

            let wasm_body = serve_one_request(&wasm, Arc::new(RecordingOps::default())).await;

            assert_eq!(wasm_body, PROBE_RESPONSE);
        })
        .await;
}

/// One cold start, from the worker row to the response body
async fn serve_one_request(data: &WorkerWithBindings, ops: OperationsHandle) -> String {
    let limits = TaskExecutionConfig::default_limits();

    let prepared = prepare_worker(data, &limits).expect("the build serves this code type");

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
