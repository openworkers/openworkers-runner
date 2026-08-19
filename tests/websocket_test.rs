//! WebSocket outgoing (client) tests
//!
//! These tests verify that workers can open outgoing WebSocket connections
//! using Cloudflare's interface: fetch() with Upgrade: websocket header.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runner::RunnerOperations;
use tokio::task::LocalSet;

use openworkers_runtime_v8::Worker;

async fn run_in_local<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let local = LocalSet::new();
    local.run_until(f()).await
}

/// Test WebSocket connect + send + receive via echo server
#[tokio::test]
async fn test_websocket_echo() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    const resp = await fetch('wss://echo.websocket.org', {
                        headers: { Upgrade: 'websocket' },
                    });

                    const ws = resp.webSocket;
                    if (!ws) {
                        event.respondWith(new Response('No webSocket on response', { status: 500 }));
                        return;
                    }

                    ws.accept();
                    ws.send('hello from openworkers');

                    const result = await new Promise((resolve, reject) => {
                        const timeout = setTimeout(() => reject(new Error('timeout')), 5000);

                        ws.addEventListener('message', (msg) => {
                            clearTimeout(timeout);
                            resolve(msg.data);
                        });

                        ws.addEventListener('error', (err) => {
                            clearTimeout(timeout);
                            reject(new Error(err.message || 'ws error'));
                        });
                    });

                    ws.close();

                    event.respondWith(new Response(JSON.stringify({ echo: result }), {
                        status: 200,
                        headers: { 'Content-Type': 'application/json' },
                    }));
                } catch (error) {
                    event.respondWith(new Response('Error: ' + error.message, { status: 500 }));
                }
            });
            "#,
        );

        let ops = Arc::new(RunnerOperations::new());
        let mut worker = Worker::new_with_ops(script, None, ops)
            .await
            .expect("Worker should initialize");

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(request);

        let exec_result = tokio::time::timeout(Duration::from_secs(10), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within timeout"
        );
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        println!("Response status: {}", response.status);
        println!("Response body: {}", body_str);

        assert_eq!(response.status, 200, "Status should be 200: {}", body_str);
        // echo.websocket.org may send a welcome message first;
        // just verify we got a JSON response with the echo field
        assert!(
            body_str.contains("echo"),
            "Should receive a response via WebSocket: {}",
            body_str
        );
    })
    .await;
}

/// Test that WebSocket class and native functions exist
#[tokio::test]
async fn test_websocket_class_exists() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                const hasClass = typeof WebSocket === 'function';
                const hasNative = typeof __nativeWebSocketConnect === 'function';
                event.respondWith(new Response(JSON.stringify({
                    hasClass,
                    hasNative,
                }), { status: 200 }));
            });
            "#,
        );

        let ops = Arc::new(RunnerOperations::new());
        let mut worker = Worker::new_with_ops(script, None, ops)
            .await
            .expect("Worker should initialize");

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(request);
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;
        assert!(exec_result.is_ok(), "Should complete within timeout");
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);
        println!("Body: {}", body_str);

        assert!(
            body_str.contains("\"hasClass\":true"),
            "WebSocket class should exist: {}",
            body_str
        );
        assert!(
            body_str.contains("\"hasNative\":true"),
            "Native function should exist: {}",
            body_str
        );
    })
    .await;
}

/// Test that WebSocket connect error is properly reported
#[tokio::test]
async fn test_websocket_connect_error() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    const resp = await fetch('wss://this-host-does-not-exist.invalid/', {
                        headers: { Upgrade: 'websocket' },
                    });

                    event.respondWith(new Response('Should not reach here', { status: 500 }));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        caught: true,
                        message: error.message,
                    }), {
                        status: 200,
                        headers: { 'Content-Type': 'application/json' },
                    }));
                }
            });
            "#,
        );

        let ops = Arc::new(RunnerOperations::new());
        let mut worker = Worker::new_with_ops(script, None, ops)
            .await
            .expect("Worker should initialize");

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(request);

        let exec_result = tokio::time::timeout(Duration::from_secs(10), worker.exec(task)).await;
        assert!(exec_result.is_ok(), "Should complete within timeout");
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        println!("Error response: {}", body_str);

        assert_eq!(response.status, 200, "Should have caught the error");
        assert!(
            body_str.contains("caught"),
            "Should report caught error: {}",
            body_str
        );
    })
    .await;
}
