//! Tests for thread-pinned pool execution
//!
//! These tests verify that execute_pinned works correctly with buffered responses.

#![cfg(feature = "v8")]

use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, RuntimeLimits, Script};
use openworkers_runner::ops::RunnerOperations;
use openworkers_runtime_v8::{PinnedExecuteRequest, execute_pinned, init_pinned_pool};
use std::collections::HashMap;
use std::sync::{Arc, Once};

static INIT: Once = Once::new();

fn init_pool() {
    INIT.call_once(|| {
        init_pinned_pool(10, RuntimeLimits::default());
    });
}

/// Helper to run async tests in a LocalSet (required for execute_pinned)
async fn run_local<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let local = tokio::task::LocalSet::new();
    local.run_until(f()).await
}

#[tokio::test]
async fn test_pinned_simple_response() {
    init_pool();

    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response('Hello, World!'));
            });"#,
        );

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(request);
        let ops = Arc::new(RunnerOperations::new());

        let result = execute_pinned(PinnedExecuteRequest {
            owner_id: "test-owner".to_string(),
            worker_id: "test-worker".to_string(),
            version: 1,
            script,
            ops,
            task,
            on_warm_hit: None,
        })
        .await;
        assert!(
            result.is_ok(),
            "execute_pinned should succeed: {:?}",
            result
        );

        let response = rx.await.expect("Should receive response");
        assert_eq!(response.status, 200);

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        println!("Response body: '{}' ({} bytes)", body_str, body.len());

        assert!(!body.is_empty(), "Body should NOT be empty!");
        assert_eq!(body_str, "Hello, World!");
    })
    .await;
}

#[tokio::test]
async fn test_pinned_html_response() {
    init_pool();

    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response('<h3>Hello world!</h3>', {
                    headers: { 'Content-Type': 'text/html' }
                }));
            });"#,
        );

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(request);
        let ops = Arc::new(RunnerOperations::new());

        let result = execute_pinned(PinnedExecuteRequest {
            owner_id: "test-owner".to_string(),
            worker_id: "test-worker".to_string(),
            version: 1,
            script,
            ops,
            task,
            on_warm_hit: None,
        })
        .await;
        assert!(
            result.is_ok(),
            "execute_pinned should succeed: {:?}",
            result
        );

        let response = rx.await.expect("Should receive response");
        assert_eq!(response.status, 200);

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        println!("Response body: '{}' ({} bytes)", body_str, body.len());

        assert!(!body.is_empty(), "Body should NOT be empty!");
        assert_eq!(body_str, "<h3>Hello world!</h3>");
    })
    .await;
}

#[tokio::test]
async fn test_pinned_json_response() {
    init_pool();

    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(Response.json({ message: 'Hello', value: 42 }));
            });"#,
        );

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(request);
        let ops = Arc::new(RunnerOperations::new());

        let result = execute_pinned(PinnedExecuteRequest {
            owner_id: "test-owner".to_string(),
            worker_id: "test-worker".to_string(),
            version: 1,
            script,
            ops,
            task,
            on_warm_hit: None,
        })
        .await;
        assert!(
            result.is_ok(),
            "execute_pinned should succeed: {:?}",
            result
        );

        let response = rx.await.expect("Should receive response");
        assert_eq!(response.status, 200);

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        println!("Response body: '{}' ({} bytes)", body_str, body.len());

        assert!(!body.is_empty(), "Body should NOT be empty!");
        assert!(body_str.contains("Hello"));
        assert!(body_str.contains("42"));
    })
    .await;
}

/// Test with globalThis.default pattern (ES module transformed)
/// This is what production code looks like after SWC transform
#[tokio::test]
async fn test_pinned_global_default_fetch() {
    init_pool();

    run_local(|| async {
        // This is what `export default { fetch() { ... } }` becomes after SWC transform
        let script = Script::new(
            r#"globalThis.default = {
                fetch(request) {
                    return new Response('hello from globalThis.default');
                }
            };"#,
        );

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(request);
        let ops = Arc::new(RunnerOperations::new());

        let result = execute_pinned(PinnedExecuteRequest {
            owner_id: "test-owner".to_string(),
            worker_id: "test-worker".to_string(),
            version: 1,
            script,
            ops,
            task,
            on_warm_hit: None,
        })
        .await;
        assert!(
            result.is_ok(),
            "execute_pinned should succeed: {:?}",
            result
        );

        let response = rx.await.expect("Should receive response");
        assert_eq!(response.status, 200);

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        println!(
            "Response body (globalThis.default): '{}' ({} bytes)",
            body_str,
            body.len()
        );

        assert!(!body.is_empty(), "Body should NOT be empty!");
        assert_eq!(body_str, "hello from globalThis.default");
    })
    .await;
}

/// Test with async fetch handler (globalThis.default pattern)
#[tokio::test]
async fn test_pinned_global_default_async_fetch() {
    init_pool();

    run_local(|| async {
        let script = Script::new(
            r#"globalThis.default = {
                async fetch(request) {
                    return new Response('<h3>async hello</h3>', {
                        headers: { 'Content-Type': 'text/html' }
                    });
                }
            };"#,
        );

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(request);
        let ops = Arc::new(RunnerOperations::new());

        let result = execute_pinned(PinnedExecuteRequest {
            owner_id: "test-owner".to_string(),
            worker_id: "test-worker".to_string(),
            version: 1,
            script,
            ops,
            task,
            on_warm_hit: None,
        })
        .await;
        assert!(
            result.is_ok(),
            "execute_pinned should succeed: {:?}",
            result
        );

        let response = rx.await.expect("Should receive response");
        assert_eq!(response.status, 200);

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        println!(
            "Response body (async globalThis.default): '{}' ({} bytes)",
            body_str,
            body.len()
        );

        assert!(!body.is_empty(), "Body should NOT be empty!");
        assert_eq!(body_str, "<h3>async hello</h3>");
    })
    .await;
}

/// REGRESSION TEST: Reproduces the production bug where buffered response bodies are empty.
///
/// This test mimics the EXACT production scenario from worker_pool.rs:
/// - LocalSet only wraps execute_pinned (not response consumption)
/// - Response body is consumed OUTSIDE the LocalSet
/// - When LocalSet drops, spawn_local tasks (like the stream forwarding task) are aborted
///
/// The bug: For buffered responses like `new Response('Hello')`, the runtime was
/// unnecessarily using streaming, spawning a forwarding task that got aborted before
/// it could send data to the response channel.
///
/// The fix: Detect buffered responses (data already queued, stream closed) and
/// bypass streaming entirely, using _getRawBody() instead.
#[tokio::test]
async fn test_buffered_response_body_not_empty_production_scenario() {
    init_pool();

    // This test reproduces the EXACT production pattern from worker_pool.rs:
    //
    // ```rust
    // let local = tokio::task::LocalSet::new();
    // local.run_until(execute_pinned(...)).await;
    // // LocalSet DROPPED here - spawn_local tasks aborted!
    // ```
    //
    // Then the HTTP server consumes the response body OUTSIDE the LocalSet.

    let script = Script::new(
        r#"addEventListener('fetch', (event) => {
            event.respondWith(new Response('Hello, World!'));
        });"#,
    );

    let request = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Event::fetch(request);
    let ops = Arc::new(RunnerOperations::new());

    // Run execute_pinned in its OWN LocalSet (like production)
    // The LocalSet is dropped immediately after execute_pinned returns
    let result = {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(execute_pinned(PinnedExecuteRequest {
                owner_id: "test-owner".to_string(),
                worker_id: "test-worker".to_string(),
                version: 1,
                script,
                ops,
                task,
                on_warm_hit: None,
            }))
            .await
        // LocalSet DROPPED here! Any spawn_local tasks are aborted!
    };

    assert!(
        result.is_ok(),
        "execute_pinned should succeed: {:?}",
        result
    );

    // Now consume the response OUTSIDE the LocalSet (like production HTTP server)
    let response = rx.await.expect("Should receive response");
    assert_eq!(response.status, 200);

    // THIS IS THE CRITICAL ASSERTION:
    // Without the fix, the body would be empty because the forwarding task was aborted
    let body = response.body.collect().await;

    // Body should NOT be None/empty for a buffered response
    let body = body.expect("Body should exist for buffered response");
    let body_str = String::from_utf8_lossy(&body);

    println!(
        "Response body (production scenario): '{}' ({} bytes)",
        body_str,
        body.len()
    );

    assert!(
        !body.is_empty(),
        "REGRESSION: Buffered response body is empty! \
         This happens when the stream forwarding task is aborted \
         before it can send data. Fix: detect buffered responses \
         and bypass streaming."
    );
    assert_eq!(body_str, "Hello, World!");
}

/// Same test but with Response.json() which also creates a buffered response
#[tokio::test]
async fn test_json_response_body_not_empty_production_scenario() {
    init_pool();

    let script = Script::new(
        r#"addEventListener('fetch', (event) => {
            event.respondWith(Response.json({ status: 'ok', count: 42 }));
        });"#,
    );

    let request = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Event::fetch(request);
    let ops = Arc::new(RunnerOperations::new());

    // Production pattern: LocalSet only wraps execute_pinned
    let result = {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(execute_pinned(PinnedExecuteRequest {
                owner_id: "test-owner".to_string(),
                worker_id: "test-worker".to_string(),
                version: 1,
                script,
                ops,
                task,
                on_warm_hit: None,
            }))
            .await
    };

    assert!(
        result.is_ok(),
        "execute_pinned should succeed: {:?}",
        result
    );

    // Consume response OUTSIDE LocalSet
    let response = rx.await.expect("Should receive response");
    assert_eq!(response.status, 200);

    let body = response.body.collect().await.expect("Body should exist");
    let body_str = String::from_utf8_lossy(&body);

    println!(
        "Response body (JSON production scenario): '{}' ({} bytes)",
        body_str,
        body.len()
    );

    assert!(!body.is_empty(), "REGRESSION: JSON response body is empty!");
    assert!(body_str.contains("ok"));
    assert!(body_str.contains("42"));
}

/// Test that actual streaming responses work when LocalSet stays alive.
///
/// Note: True streaming responses (user-provided ReadableStream) in the PRODUCTION
/// scenario (LocalSet dropped after execute_pinned) would fail because the
/// forwarding task is spawned via spawn_local and gets aborted.
///
/// This is a KNOWN LIMITATION. The current fix only addresses BUFFERED responses.
/// True streaming with early LocalSet drop needs a different solution:
/// - Option 1: Use tokio::spawn instead of spawn_local (requires Send bounds)
/// - Option 2: Wait for forwarding task in exec() before returning
/// - Option 3: Keep LocalSet alive until response is consumed (production does this via WORKER_POOL)
///
/// For now, we test that streaming works when LocalSet stays alive (like run_local does).
#[tokio::test]
async fn test_streaming_response_with_localset_alive() {
    init_pool();

    // Test streaming when LocalSet stays alive (normal test pattern)
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                const encoder = new TextEncoder();
                const stream = new ReadableStream({
                    start(controller) {
                        controller.enqueue(encoder.encode('chunk1'));
                        controller.enqueue(encoder.encode('chunk2'));
                        controller.close();
                    }
                });
                event.respondWith(new Response(stream));
            });"#,
        );

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(request);
        let ops = Arc::new(RunnerOperations::new());

        let result = execute_pinned(PinnedExecuteRequest {
            owner_id: "test-owner".to_string(),
            worker_id: "test-worker".to_string(),
            version: 1,
            script,
            ops,
            task,
            on_warm_hit: None,
        })
        .await;
        assert!(
            result.is_ok(),
            "execute_pinned should succeed: {:?}",
            result
        );

        let response = rx.await.expect("Should receive response");
        assert_eq!(response.status, 200);

        let body = response.body.collect().await.expect("Body should exist");
        let body_str = String::from_utf8_lossy(&body);

        println!(
            "Response body (streaming with LocalSet alive): '{}' ({} bytes)",
            body_str,
            body.len()
        );

        assert!(
            !body.is_empty(),
            "Streaming response body should not be empty"
        );
        assert_eq!(body_str, "chunk1chunk2");
    })
    .await;
}
