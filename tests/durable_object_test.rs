//! A Durable Object keeps its storage in its SQLite file across instances.

#![cfg(feature = "v8")]

use std::collections::HashMap;
use std::sync::{Arc, Once};
use std::time::Duration;

use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runner::durable::id::IdKey;
use openworkers_runner::durable::{NamespaceConfig, ObjectSource, send};
use openworkers_runner::ops::RunnerOperations;

static INIT: Once = Once::new();

fn init_d1() {
    INIT.call_once(|| {
        let dir = std::env::temp_dir().join(format!("do-runner-test-{}", uuid::Uuid::new_v4()));

        // SAFETY: set once, before any thread of this test binary reads it
        unsafe { std::env::set_var("D1_DATA_DIR", &dir) };

        futures::executor::block_on(openworkers_runner::d1::init(None)).unwrap();
    });
}

const VISITS: &str = r#"
    export class Visits {
        constructor(state) {
            this.state = state;
        }

        async fetch() {
            const count = ((await this.state.storage.get('count')) ?? 0) + 1;
            this.state.storage.put('count', count);
            this.first ??= count;

            return new Response(`${count} ${this.first}`);
        }
    }
"#;

fn new_namespace() -> NamespaceConfig {
    NamespaceConfig {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user".to_string(),
        worker_id: "worker".to_string(),
        class_name: "Visits".to_string(),
        key: IdKey([3; 32]),
    }
}

/// Sends a request to the object of `name`, with the storage the runner
/// gives it.
async fn visit(namespace: &NamespaceConfig, name: &str) -> String {
    let id = openworkers_runner::durable::id::from_name(&namespace.key, name);
    let served = namespace.clone();
    let object_id = id.clone();
    let load = move |depths| async move {
        let ops = RunnerOperations::new().for_object(served, object_id, depths);
        let code = openworkers_transform::parse_worker_code(
            VISITS.as_bytes(),
            openworkers_transform::CodeLanguage::JavaScript,
        )
        .unwrap();

        Ok(ObjectSource {
            script: Script::new(code),
            ops: Arc::new(ops),
            version: 1,
            limits: openworkers_runner::task_executor::TaskExecutionConfig::default_limits(),
            idle: Duration::from_millis(100),
            drain: openworkers_runner::durable::DRAIN,
        })
    };

    let request = HttpRequest {
        method: HttpMethod::Get,
        url: "http://object/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let response = tokio::time::timeout(
        Duration::from_secs(10),
        send(namespace, id, 0, request, load),
    )
    .await
    .expect("the object answers in time")
    .unwrap();

    let body = response.response.body.collect().await.unwrap().unwrap();

    String::from_utf8(body.to_vec()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_storage_outlives_the_instance() {
    init_d1();
    let namespace = new_namespace();

    assert_eq!(visit(&namespace, "a").await, "1 1");
    assert_eq!(visit(&namespace, "a").await, "2 1");

    // Past the idle time, a new instance reads what the first one wrote
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(visit(&namespace, "a").await, "3 3");
}

#[tokio::test(flavor = "multi_thread")]
async fn each_object_has_its_own_storage() {
    init_d1();
    let namespace = new_namespace();
    let other = new_namespace();

    assert_eq!(visit(&namespace, "a").await, "1 1");
    assert_eq!(visit(&namespace, "a").await, "2 1");
    assert_eq!(visit(&namespace, "b").await, "1 1");
    assert_eq!(visit(&other, "a").await, "1 1");
}
