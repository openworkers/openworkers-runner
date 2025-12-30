//! Fetch streaming tests
//!
//! These tests verify that workers can make outgoing fetch() requests
//! with streaming support. They use RunnerOperations which provides
//! real HTTP via reqwest.
//!
//! Note: These tests require network access to httpbin.workers.rocks

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task};
use openworkers_runner::RunnerOperations;
use tokio::task::LocalSet;

// Re-export the runtime's Worker type (depends on feature flag)
#[cfg(feature = "v8")]
use openworkers_runtime_v8::Worker;

/// Helper to run async tests in a LocalSet (required for tokio 1.48+ spawn_local)
async fn run_in_local<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let local = LocalSet::new();
    local.run_until(f()).await
}

/// Test basic fetch - worker fetches external URL and returns result
#[tokio::test(flavor = "current_thread")]
async fn test_fetch_basic() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    const response = await fetch('https://httpbin.workers.rocks/get');
                    const data = await response.text();
                    event.respondWith(new Response(data, {
                        status: response.status,
                        headers: { 'Content-Type': 'application/json' }
                    }));
                } catch (error) {
                    event.respondWith(new Response('Fetch error: ' + error.message, { status: 500 }));
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

        let (task, rx) = Task::fetch(request);

        // Execute with timeout since this makes a network request
        let exec_result = tokio::time::timeout(Duration::from_secs(30), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within timeout"
        );
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        assert_eq!(response.status, 200, "Status should be 200");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        // httpbin.workers.rocks/get returns JSON with request info
        assert!(
            body_str.contains("headers") || body_str.contains("url"),
            "Body should contain request info: {}",
            body_str
        );
    })
    .await;
}

/// Test fetch with streaming body read
#[tokio::test(flavor = "current_thread")]
async fn test_fetch_streaming_read() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    const response = await fetch('https://httpbin.workers.rocks/bytes/1024');
                    const reader = response.body.getReader();
                    let totalBytes = 0;
                    let chunkCount = 0;

                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        chunkCount++;
                        totalBytes += value.length;
                    }

                    event.respondWith(new Response(JSON.stringify({
                        totalBytes,
                        chunkCount,
                        success: true
                    }), {
                        headers: { 'Content-Type': 'application/json' }
                    }));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        success: false
                    }), { status: 500 }));
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

        let (task, rx) = Task::fetch(request);

        let exec_result = tokio::time::timeout(Duration::from_secs(30), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within timeout"
        );
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        assert_eq!(response.status, 200, "Status should be 200");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        // Parse the JSON response
        assert!(
            body_str.contains("\"success\":true"),
            "Should have success: {}",
            body_str
        );
        assert!(
            body_str.contains("\"totalBytes\":1024"),
            "Should have 1024 bytes: {}",
            body_str
        );
        assert!(
            body_str.contains("\"chunkCount\":"),
            "Should have chunk count: {}",
            body_str
        );
    })
    .await;
}

/// Test fetch forward - proxy pattern
#[tokio::test(flavor = "current_thread")]
async fn test_fetch_forward() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    // Proxy pattern: fetch upstream and forward response
                    const upstream = await fetch('https://httpbin.workers.rocks/headers');

                    // Create new response with upstream body
                    const body = await upstream.text();
                    event.respondWith(new Response(body, {
                        status: upstream.status,
                        headers: upstream.headers
                    }));
                } catch (error) {
                    event.respondWith(new Response('Forward error: ' + error.message, { status: 500 }));
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

        let (task, rx) = Task::fetch(request);

        let exec_result = tokio::time::timeout(Duration::from_secs(30), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within timeout"
        );
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        assert_eq!(response.status, 200, "Status should be 200");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        // httpbin.workers.rocks/headers returns request headers as JSON
        assert!(
            body_str.contains("headers") || body_str.contains("Host"),
            "Body should contain headers info: {}",
            body_str
        );
    })
    .await;
}

/// Test POST fetch with body
#[tokio::test(flavor = "current_thread")]
async fn test_fetch_post_with_body() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    const response = await fetch('https://httpbin.workers.rocks/post', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify({ test: 'data', value: 42 })
                    });

                    const data = await response.json();
                    event.respondWith(new Response(JSON.stringify({
                        status: response.status,
                        receivedData: data.data || data.json,
                        success: true
                    }), {
                        headers: { 'Content-Type': 'application/json' }
                    }));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        success: false
                    }), { status: 500 }));
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

        let (task, rx) = Task::fetch(request);

        let exec_result = tokio::time::timeout(Duration::from_secs(30), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within timeout"
        );
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        assert_eq!(response.status, 200, "Status should be 200");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        assert!(
            body_str.contains("\"success\":true"),
            "Should have success: {}",
            body_str
        );
    })
    .await;
}

/// Test that fetch forward returns a streaming response
#[tokio::test(flavor = "current_thread")]
async fn test_fetch_forward_streaming() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', (event) => {
                // Direct fetch forward - body should be a native stream
                event.respondWith(fetch('https://httpbin.workers.rocks/bytes/100'));
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

        let (task, rx) = Task::fetch(request);
        let _result = tokio::time::timeout(Duration::from_secs(30), worker.exec(task)).await;

        // Wait for response with timeout
        let response = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("Timeout waiting for response")
            .expect("Channel error");

        assert_eq!(response.status, 200);

        // The response body should be a stream (not bytes)
        assert!(
            response.body.is_stream(),
            "Fetch forward should return streaming body"
        );

        // Consume the stream and verify we got 100 bytes
        let body = response.body.collect().await.expect("Should have body");
        assert_eq!(
            body.len(),
            100,
            "Should have received 100 bytes from /bytes/100"
        );
    })
    .await;
}

/// Test streaming response with chunked reading
#[tokio::test(flavor = "current_thread")]
async fn test_streaming_response_chunked() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', (event) => {
                // Fetch stream endpoint - should receive multiple chunks
                event.respondWith(fetch('https://httpbin.workers.rocks/stream/3'));
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

        let (task, rx) = Task::fetch(request);
        let _result = tokio::time::timeout(Duration::from_secs(30), worker.exec(task)).await;

        let response = tokio::time::timeout(Duration::from_secs(15), rx)
            .await
            .expect("Timeout")
            .expect("Channel error");

        assert_eq!(response.status, 200);
        assert!(response.body.is_stream(), "Should be streaming");

        // Consume the stream - /stream/3 returns 3 JSON objects
        let body = response.body.collect().await.expect("Should have body");
        assert!(!body.is_empty(), "Should have received data");
    })
    .await;
}

/// Test that processed fetch (not forward) still works
#[tokio::test(flavor = "current_thread")]
async fn test_processed_fetch_response() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                // Fetch but process the response (consume it)
                const upstream = await fetch('https://httpbin.workers.rocks/get');
                const text = await upstream.text();

                // Return a new response with processed content
                event.respondWith(new Response('Processed: ' + text.substring(0, 20)));
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

        let (task, rx) = Task::fetch(request);
        let _result = tokio::time::timeout(Duration::from_secs(30), worker.exec(task)).await;

        let response = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("Timeout")
            .expect("Channel error");

        assert_eq!(response.status, 200);

        // All responses with body should be streamed
        assert!(
            response.body.is_stream(),
            "Processed response should be streamed"
        );

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.starts_with("Processed:"), "Body: {}", body_str);
    })
    .await;
}

/// Test fetch() with a ReadableStream as body
/// This is a critical edge case: streaming body in outgoing fetch requests
#[tokio::test(flavor = "current_thread")]
async fn test_fetch_with_streaming_body() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                // Create a ReadableStream with chunked data
                const chunks = ['Hello, ', 'streaming ', 'world!'];
                let index = 0;
                const stream = new ReadableStream({
                    pull(controller) {
                        if (index < chunks.length) {
                            controller.enqueue(new TextEncoder().encode(chunks[index]));
                            index++;
                        } else {
                            controller.close();
                        }
                    }
                });

                // Use the stream as body in a fetch request
                const response = await fetch('https://httpbin.workers.rocks/post', {
                    method: 'POST',
                    body: stream,
                    headers: { 'Content-Type': 'text/plain' }
                });

                const result = await response.json();
                event.respondWith(new Response(JSON.stringify({
                    success: true,
                    received: result.data
                })));
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

        let (task, rx) = Task::fetch(request);

        // Use timeout since this makes a network request
        let exec_result = tokio::time::timeout(Duration::from_secs(30), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within timeout"
        );
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        assert_eq!(response.status, 200);

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        assert!(
            body_str.contains("success"),
            "Body should contain success: {}",
            body_str
        );
        assert!(
            body_str.contains("Hello, streaming world!"),
            "Body should contain streamed data: {}",
            body_str
        );
    })
    .await;
}
