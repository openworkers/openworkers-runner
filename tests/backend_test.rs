//! Backend-agnostic checks: whichever backend the build selected must honour the
//! runner's script contract, or refuse the worker outright.

use openworkers_core::BindingType;
use openworkers_runner::runtime;
use openworkers_runner::store::{Binding, CodeType, StorageConfig, WorkerWithBindings};
use openworkers_runner::worker::prepare_script;
use std::collections::HashMap;

fn worker(code: &str, code_type: CodeType, bindings: Vec<Binding>) -> WorkerWithBindings {
    WorkerWithBindings {
        id: "test-worker".to_string(),
        name: Some("test".to_string()),
        user_id: "test-user".to_string(),
        code: code.as_bytes().to_vec(),
        code_type,
        version: 1,
        env: HashMap::new(),
        bindings,
        env_updated_at: None,
    }
}

fn native_code_type() -> CodeType {
    if runtime::RUNS_JAVASCRIPT {
        CodeType::Javascript
    } else {
        CodeType::Wasm
    }
}

fn native_backend() -> runtime::Backend {
    if runtime::RUNS_JAVASCRIPT {
        runtime::Backend::Js
    } else {
        runtime::Backend::Wasm
    }
}

fn assets_binding() -> Binding {
    Binding::Assets {
        key: "ASSETS".to_string(),
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
    }
}

#[test]
fn a_worker_without_bindings_is_accepted() {
    let script =
        prepare_script(&worker("", native_code_type(), vec![])).expect("script should prepare");

    assert!(script.bindings.is_empty());
}

/// A backend that drops a binding type must say so, not hand the guest an
/// undefined `env`.
#[test]
fn bindings_are_refused_unless_the_backend_implements_their_type() {
    let result = prepare_script(&worker("", native_code_type(), vec![assets_binding()]));

    if native_backend().supports_binding(BindingType::Assets) {
        let script = result.expect("the backend wires assets bindings");
        assert_eq!(script.bindings.len(), 1);
    } else {
        let err = format!("{:?}", result.expect_err("the backend would drop ASSETS"));

        assert!(err.contains("assets"), "the type is not named: {err}");
        assert!(err.contains("ASSETS"), "the binding is not named: {err}");
    }
}

#[cfg(feature = "_js")]
#[tokio::test]
async fn the_selected_backend_serves_a_fetch() {
    use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody};
    use openworkers_runner::ops::RunnerOperations;
    use openworkers_runner::task_executor::TaskExecutionConfig;
    use openworkers_runner::worker::create_worker;
    use std::sync::Arc;

    let data = worker(
        "addEventListener('fetch', (event) => event.respondWith(new Response('OK')));",
        CodeType::Javascript,
        vec![],
    );

    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
            let script = prepare_script(&data).expect("script should prepare");
            let ops = Arc::new(RunnerOperations::new());

            let mut w = create_worker(script, TaskExecutionConfig::default_limits(), ops)
                .await
                .expect("worker should initialize");

            let (event, rx) = Event::fetch(HttpRequest {
                method: HttpMethod::Get,
                url: "http://localhost/".to_string(),
                headers: HashMap::new(),
                body: RequestBody::None,
            });

            w.exec(event).await.expect("fetch should run");

            let response = rx.await.expect("worker should respond");
            assert_eq!(response.status, 200);

            let body = response
                .body
                .collect()
                .await
                .expect("body stream failed")
                .expect("response has a body");
            assert_eq!(String::from_utf8_lossy(&body), "OK");
        })
        .await;
}
