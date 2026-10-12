use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use openworkers_core::{
    HttpMethod, HttpRequest, ObjectIdSource, OperationsHandler, RequestBody, Script,
};

use super::id::IdKey;
use super::registry::is_running;
use super::{NamespaceConfig, ObjectSource, send};
use crate::ops::RunnerOperations;
use crate::store::Binding;

const COUNTER: &str = r#"
    export class Counter {
        constructor(state) {
            this.state = state;
            this.count = 0;
        }

        async fetch(request) {
            this.count += 1;
            await new Promise((resolve) => setTimeout(resolve, 1));
            return new Response(`${this.state.id} ${this.count}`);
        }
    }
"#;

fn namespace(id: &str) -> NamespaceConfig {
    NamespaceConfig {
        id: id.to_string(),
        user_id: "user".to_string(),
        worker_id: "worker".to_string(),
        class_name: "Counter".to_string(),
        key: IdKey([1; 32]),
    }
}

fn source(idle: Duration) -> ObjectSource {
    let code = openworkers_transform::parse_worker_code(
        COUNTER.as_bytes(),
        openworkers_transform::CodeLanguage::JavaScript,
    )
    .unwrap();

    ObjectSource {
        script: Script::new(code),
        ops: Arc::new(RunnerOperations::new()),
        limits: crate::task_executor::TaskExecutionConfig::default_limits(),
        idle,
    }
}

fn request() -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: "http://object/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    }
}

/// Sends a request to the object `id`, and counts the loads in `loads`.
async fn get(
    namespace: &NamespaceConfig,
    id: &str,
    idle: Duration,
    loads: &Arc<AtomicUsize>,
) -> Result<String, String> {
    let loads = Arc::clone(loads);
    let load = move || async move {
        loads.fetch_add(1, Ordering::SeqCst);
        Ok(source(idle))
    };

    let response = tokio::time::timeout(
        Duration::from_secs(10),
        send(namespace, id.to_string(), request(), load),
    )
    .await
    .expect("the object answers in time")?;

    let body = response.body.collect().await.unwrap().unwrap();

    Ok(String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_object_keeps_its_state_and_each_id_has_its_own() {
    let namespace = namespace("ns-state");
    let loads = Arc::new(AtomicUsize::new(0));
    let idle = Duration::from_secs(60);

    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 1");
    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 2");
    assert_eq!(get(&namespace, "b", idle, &loads).await.unwrap(), "b 1");
    assert_eq!(loads.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_requests_start_one_object() {
    let namespace = namespace("ns-burst");
    let loads = Arc::new(AtomicUsize::new(0));
    let idle = Duration::from_secs(60);

    let answers =
        futures::future::join_all((0..20).map(|_| get(&namespace, "a", idle, &loads))).await;

    // The events interleave at their await, so each one reads the count
    // after some of the others
    for answer in answers {
        answer.unwrap();
    }

    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 21");
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_object_leaves_and_a_request_starts_it_again() {
    let namespace = namespace("ns-idle");
    let loads = Arc::new(AtomicUsize::new(0));
    let idle = Duration::from_millis(100);

    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 1");
    assert!(is_running("ns-idle", "a"));

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!is_running("ns-idle", "a"));

    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 1");
    assert_eq!(loads.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_load_fails_the_request_and_the_next_one_loads_again() {
    let namespace = namespace("ns-load");
    let loads = Arc::new(AtomicUsize::new(0));

    let failing = send(&namespace, "a".to_string(), request(), || async {
        Err("no worker".to_string())
    })
    .await;

    assert_eq!(failing.unwrap_err(), "the Durable Object stopped");
    assert_eq!(
        get(&namespace, "a", Duration::from_secs(60), &loads)
            .await
            .unwrap(),
        "a 1"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_namespace_ends_its_objects_and_refuses_requests() {
    let namespace = namespace("ns-stopped");
    let loads = Arc::new(AtomicUsize::new(0));
    let idle = Duration::from_secs(60);

    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 1");

    // The object is busy with nothing: stopping it closes its mailbox, and
    // the task ends soon after
    super::stop_namespace("ns-stopped");

    let mut stopped = false;

    for _ in 0..50 {
        if super::stop_namespace("ns-stopped") {
            stopped = true;
            break;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(stopped, "the object still runs");
    assert!(!is_running("ns-stopped", "a"));
    assert_eq!(
        get(&namespace, "a", idle, &loads).await.unwrap_err(),
        "the Durable Object namespace was deleted"
    );
}

fn ops_with_namespace() -> RunnerOperations {
    RunnerOperations::new().with_bindings(vec![Binding::DurableObject {
        key: "COUNTER".to_string(),
        namespace: namespace("ns-ops"),
    }])
}

#[test]
fn the_binding_makes_ids_of_its_namespace() {
    let ops = ops_with_namespace();
    let key = IdKey([1; 32]);

    let named = ops
        .durable_object_id("COUNTER", ObjectIdSource::Name("a".to_string()))
        .unwrap();
    assert_eq!(named, super::id::from_name(&key, "a"));

    let unique = ops
        .durable_object_id("COUNTER", ObjectIdSource::Unique)
        .unwrap();
    assert_eq!(super::id::parse(&key, &unique), Some(unique.clone()));

    assert_eq!(
        ops.durable_object_id("COUNTER", ObjectIdSource::Text(named.to_uppercase())),
        Ok(named)
    );
    assert_eq!(
        ops.durable_object_id("COUNTER", ObjectIdSource::Text("a".to_string())),
        Err("Invalid Durable Object ID".to_string())
    );
    assert_eq!(
        ops.durable_object_id("OTHER", ObjectIdSource::Unique),
        Err("Durable Object binding 'OTHER' not configured".to_string())
    );
}

#[tokio::test]
async fn the_binding_refuses_an_id_it_did_not_make() {
    let ops = ops_with_namespace();
    let other = super::id::from_name(&IdKey([2; 32]), "a");

    let refused = ops
        .handle_binding_durable_object("COUNTER", &other, request())
        .await;
    assert_eq!(refused.unwrap_err(), "Invalid Durable Object ID");

    let unknown = ops
        .handle_binding_durable_object("OTHER", &other, request())
        .await;
    assert_eq!(
        unknown.unwrap_err(),
        "Durable Object binding 'OTHER' not configured"
    );
}

#[tokio::test]
async fn an_object_needs_the_database_to_load_its_worker() {
    let ops = ops_with_namespace();
    let id = super::id::from_name(&IdKey([1; 32]), "a");

    let answer = ops
        .handle_binding_durable_object("COUNTER", &id, request())
        .await;

    assert_eq!(answer.unwrap_err(), "the Durable Object stopped");
}
