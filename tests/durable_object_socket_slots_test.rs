//! The WebSockets of Durable Objects take the socket slots of the runner.
//! One test in its own binary, as it sets the count of slots.

#![cfg(feature = "v8")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runner::durable::id::IdKey;
use openworkers_runner::durable::sockets::Reload;
use openworkers_runner::durable::{NamespaceConfig, ObjectSource, send};
use openworkers_runner::ops::RunnerOperations;

const ROOM: &str = r#"
    export class Room {
        constructor(state) {
            this.state = state;
        }

        async fetch() {
            const [client, server] = Object.values(new WebSocketPair());
            this.state.acceptWebSocket(server);

            return new Response(null, { status: 101, webSocket: client });
        }
    }
"#;

fn room(namespace: NamespaceConfig, id: String) -> Reload {
    Arc::new(move |depths| {
        let namespace = namespace.clone();
        let id = id.clone();

        Box::pin(async move {
            let reload = room(namespace.clone(), id.clone());
            let ops = RunnerOperations::new()
                .for_object(namespace, id, depths)
                .reloading_with(reload);
            let code = openworkers_transform::parse_worker_code(
                ROOM.as_bytes(),
                openworkers_transform::CodeLanguage::JavaScript,
            )
            .unwrap();

            Ok(ObjectSource {
                script: Script::new(code),
                ops: Arc::new(ops),
                version: 1,
                limits: openworkers_runner::task_executor::TaskExecutionConfig::default_limits(),
                idle: Duration::from_secs(60),
                drain: openworkers_runner::durable::DRAIN,
            })
        })
    })
}

/// An upgrade to the object; answers its status and the end that hands
/// the socket of the client over.
async fn upgrade(
    namespace: &NamespaceConfig,
    id: &str,
) -> (
    u16,
    Option<tokio::sync::oneshot::Sender<openworkers_core::WebSocketConnection>>,
) {
    let request = HttpRequest {
        method: HttpMethod::Get,
        url: "http://object/".to_string(),
        headers: HashMap::from([("upgrade".to_string(), "websocket".to_string())]),
        body: RequestBody::None,
    };
    let reload = room(namespace.clone(), id.to_string());

    let answer = tokio::time::timeout(
        Duration::from_secs(10),
        send(namespace, id.to_string(), 0, request, move |depths| {
            reload(depths)
        }),
    )
    .await
    .expect("the object answers in time")
    .unwrap();

    (answer.response.status, answer.socket)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sockets_of_objects_take_socket_slots() {
    // SAFETY: the only test of this binary, before anything reads it
    unsafe { std::env::set_var("MAX_WS_CONNECTIONS", "2") };

    let namespace = NamespaceConfig {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user".to_string(),
        worker_id: "worker".to_string(),
        class_name: "Room".to_string(),
        key: IdKey([5; 32]),
    };
    let id = openworkers_runner::durable::id::from_name(&namespace.key, "a");

    let (_, first) = upgrade(&namespace, &id).await;
    let (_, second) = upgrade(&namespace, &id).await;
    let (_, third) = upgrade(&namespace, &id).await;
    let (first, second, mut third) = (first.unwrap(), second.unwrap(), third.unwrap());

    // The object answers 101, and the socket over the slots closes at once
    tokio::time::timeout(Duration::from_secs(5), third.closed())
        .await
        .expect("a socket over the slots closes");
    assert!(!first.is_closed() && !second.is_closed());

    // A socket that ends gives its slot back
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (_, fourth) = upgrade(&namespace, &id).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!fourth.unwrap().is_closed());
}
