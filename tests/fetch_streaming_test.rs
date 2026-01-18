//! Fetch streaming tests
//!
//! These tests verify that workers can make outgoing fetch() requests
//! with streaming support. They use RunnerOperations which provides
//! real HTTP via reqwest.
//!
//! Note: These tests require network access to httpbin.workers.rocks

#![cfg(not(feature = "wasm"))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
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
#[tokio::test]
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

        let (task, rx) = Event::fetch(request);

        // Execute with timeout since this makes a network request
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

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
#[tokio::test]
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

        let (task, rx) = Event::fetch(request);

        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

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
#[tokio::test]
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

        let (task, rx) = Event::fetch(request);

        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

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
#[tokio::test]
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

        let (task, rx) = Event::fetch(request);

        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

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
#[tokio::test]
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

        let (task, rx) = Event::fetch(request);
        let _result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        // Wait for response with timeout
        let response = tokio::time::timeout(Duration::from_secs(5), rx)
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
#[tokio::test]
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

        let (task, rx) = Event::fetch(request);
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;
        if let Ok(Err(e)) = &exec_result {
            eprintln!("EXEC_ERROR: {:?}", e);
        }
        let _result = exec_result;

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
#[tokio::test]
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

        let (task, rx) = Event::fetch(request);
        let _result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        let response = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("Timeout")
            .expect("Channel error");

        assert_eq!(response.status, 200);

        // Buffered responses (string body) use ResponseBody::Bytes, not Stream
        // Only true streaming responses (user-provided ReadableStream) use Stream
        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.starts_with("Processed:"), "Body: {}", body_str);
    })
    .await;
}

/// Test Response.clone() after fetch
/// This tests the tee() implementation used by clone()
/// Bug report: Response.clone() causes worker to hang in caching scenarios
#[tokio::test]
async fn test_response_clone_after_fetch() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    const response = await fetch('https://httpbin.workers.rocks/get');

                    // Clone the response (this uses tee() internally)
                    const cloned = response.clone();

                    // Read from the clone
                    const clonedText = await cloned.text();

                    // Read from the original
                    const originalText = await response.text();

                    event.respondWith(new Response(JSON.stringify({
                        success: true,
                        clonedLength: clonedText.length,
                        originalLength: originalText.length,
                        match: clonedText === originalText
                    }), {
                        headers: { 'Content-Type': 'application/json' }
                    }));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        stack: error.stack,
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

        let (task, rx) = Event::fetch(request);

        // Use timeout - this is where the hang would occur
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within timeout (Response.clone() may be hanging)"
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
        assert!(
            body_str.contains("\"match\":true"),
            "Cloned and original should match: {}",
            body_str
        );
    })
    .await;
}

/// Test Response.clone() in caching scenario
/// Reproduces the exact bug: cache response, return clone
#[tokio::test]
async fn test_response_clone_cache_pattern() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            const cacheMap = new Map();

            async function cachedFetch(url) {
                const cached = cacheMap.get(url);

                if (cached) {
                    // Return clone of cached response
                    return cached.clone();
                }

                const res = await fetch(url);
                cacheMap.set(url, res);

                // Return clone, keep original in cache
                return res.clone();
            }

            addEventListener('fetch', async (event) => {
                try {
                    const url = 'https://httpbin.workers.rocks/get';

                    // First call - should fetch and cache
                    const res1 = await cachedFetch(url);
                    const text1 = await res1.text();

                    // Second call - should return clone from cache
                    const res2 = await cachedFetch(url);
                    const text2 = await res2.text();

                    event.respondWith(new Response(JSON.stringify({
                        success: true,
                        firstLength: text1.length,
                        secondLength: text2.length,
                        match: text1 === text2
                    }), {
                        headers: { 'Content-Type': 'application/json' }
                    }));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        stack: error.stack,
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

        let (task, rx) = Event::fetch(request);

        // This is where the hang occurs - timeout will catch it
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within timeout (caching with clone() may be hanging)"
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

/// Test fetch() with a ReadableStream as body
/// This is a critical edge case: streaming body in outgoing fetch requests
#[tokio::test]
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

        let (task, rx) = Event::fetch(request);

        // Use timeout since this makes a network request
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

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

/// Test basic tee() without fetch - isolates the stream issue
#[tokio::test]
async fn test_basic_stream_tee() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    // Create a simple ReadableStream
                    const stream = new ReadableStream({
                        start(controller) {
                            controller.enqueue(new TextEncoder().encode('Hello '));
                            controller.enqueue(new TextEncoder().encode('World!'));
                            controller.close();
                        }
                    });

                    // Tee the stream
                    const [branch1, branch2] = stream.tee();

                    // Read from branch2 first (the clone scenario)
                    const reader2 = branch2.getReader();
                    let text2 = '';
                    while (true) {
                        const { done, value } = await reader2.read();
                        if (done) break;
                        text2 += new TextDecoder().decode(value);
                    }

                    // Read from branch1
                    const reader1 = branch1.getReader();
                    let text1 = '';
                    while (true) {
                        const { done, value } = await reader1.read();
                        if (done) break;
                        text1 += new TextDecoder().decode(value);
                    }

                    event.respondWith(new Response(JSON.stringify({
                        success: true,
                        text1,
                        text2,
                        match: text1 === text2
                    }), {
                        headers: { 'Content-Type': 'application/json' }
                    }));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        stack: error.stack,
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

        let (task, rx) = Event::fetch(request);

        // This should be quick since no network is involved
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within timeout (basic tee() may be hanging)"
        );
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(2), rx)
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
        assert!(
            body_str.contains("Hello World!"),
            "Should contain message: {}",
            body_str
        );
    })
    .await;
}

/// Test basic ReadableStream without tee - sanity check
#[tokio::test]
async fn test_basic_stream_no_tee() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    // Create a simple ReadableStream
                    const stream = new ReadableStream({
                        start(controller) {
                            controller.enqueue(new TextEncoder().encode('Hello '));
                            controller.enqueue(new TextEncoder().encode('World!'));
                            controller.close();
                        }
                    });

                    // Read from stream directly (no tee)
                    const reader = stream.getReader();
                    let text = '';
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        text += new TextDecoder().decode(value);
                    }

                    event.respondWith(new Response(JSON.stringify({
                        success: true,
                        text
                    }), {
                        headers: { 'Content-Type': 'application/json' }
                    }));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        stack: error.stack,
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

        let (task, rx) = Event::fetch(request);

        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within timeout"
        );
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(2), rx)
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
        assert!(
            body_str.contains("Hello World!"),
            "Should contain message: {}",
            body_str
        );
    })
    .await;
}

/// Test tee() reading from branch1 only
#[tokio::test]
async fn test_tee_branch1_only() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    const stream = new ReadableStream({
                        start(controller) {
                            controller.enqueue(new TextEncoder().encode('Hello'));
                            controller.close();
                        }
                    });

                    const [branch1, branch2] = stream.tee();

                    // Only read from branch1
                    const reader1 = branch1.getReader();
                    let text = '';
                    while (true) {
                        const { done, value } = await reader1.read();
                        if (done) break;
                        text += new TextDecoder().decode(value);
                    }

                    event.respondWith(new Response(JSON.stringify({
                        success: true,
                        text
                    })));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        stack: error.stack
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

        let (task, rx) = Event::fetch(request);
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(exec_result.is_ok(), "Reading from branch1 only should work");
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        assert!(
            body_str.contains("Hello"),
            "Should contain message: {}",
            body_str
        );
    })
    .await;
}

/// Test tee() reading from branch2 only
#[tokio::test]
async fn test_tee_branch2_only() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    const stream = new ReadableStream({
                        start(controller) {
                            controller.enqueue(new TextEncoder().encode('Hello'));
                            controller.close();
                        }
                    });

                    const [branch1, branch2] = stream.tee();

                    // Only read from branch2
                    const reader2 = branch2.getReader();
                    let text = '';
                    while (true) {
                        const { done, value } = await reader2.read();
                        if (done) break;
                        text += new TextDecoder().decode(value);
                    }

                    event.respondWith(new Response(JSON.stringify({
                        success: true,
                        text
                    })));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        stack: error.stack
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

        let (task, rx) = Event::fetch(request);
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(exec_result.is_ok(), "Reading from branch2 only should work");
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        assert!(
            body_str.contains("Hello"),
            "Should contain message: {}",
            body_str
        );
    })
    .await;
}

/// Test ReadableStream with async pull
#[tokio::test]
async fn test_stream_async_pull() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    let count = 0;
                    const stream = new ReadableStream({
                        async pull(controller) {
                            count++;
                            if (count <= 2) {
                                controller.enqueue(new TextEncoder().encode('chunk' + count));
                            } else {
                                controller.close();
                            }
                        }
                    });

                    const reader = stream.getReader();
                    let text = '';
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        text += new TextDecoder().decode(value) + ',';
                    }

                    event.respondWith(new Response(JSON.stringify({
                        success: true,
                        text
                    })));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        stack: error.stack
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

        let (task, rx) = Event::fetch(request);
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(exec_result.is_ok(), "Async pull should work");
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        assert!(
            body_str.contains("chunk1") && body_str.contains("chunk2"),
            "Should contain chunks: {}",
            body_str
        );
    })
    .await;
}

/// Test ReadableStream with async pull that awaits another stream
#[tokio::test]
async fn test_stream_async_pull_nested() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    // Source stream with data
                    const source = new ReadableStream({
                        start(controller) {
                            controller.enqueue(new TextEncoder().encode('source1'));
                            controller.enqueue(new TextEncoder().encode('source2'));
                            controller.close();
                        }
                    });
                    const sourceReader = source.getReader();

                    // Wrapper stream that pulls from source
                    const wrapper = new ReadableStream({
                        async pull(controller) {
                            const { done, value } = await sourceReader.read();
                            if (done) {
                                controller.close();
                            } else {
                                controller.enqueue(value);
                            }
                        }
                    });

                    const reader = wrapper.getReader();
                    let text = '';
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        text += new TextDecoder().decode(value) + ',';
                    }

                    event.respondWith(new Response(JSON.stringify({
                        success: true,
                        text
                    })));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        stack: error.stack
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

        let (task, rx) = Event::fetch(request);
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(exec_result.is_ok(), "Nested async pull should work");
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        assert!(
            body_str.contains("source1") && body_str.contains("source2"),
            "Should contain sources: {}",
            body_str
        );
    })
    .await;
}

/// Test two streams sharing a reader (similar to tee)
#[tokio::test]
async fn test_two_streams_shared_reader() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    // Source stream with data
                    const source = new ReadableStream({
                        start(controller) {
                            controller.enqueue(new TextEncoder().encode('A'));
                            controller.enqueue(new TextEncoder().encode('B'));
                            controller.close();
                        }
                    });
                    const sourceReader = source.getReader();
                    let reading = false;
                    let closedOrErrored = false;

                    // Create two streams that share the same source reader
                    const stream1 = new ReadableStream({
                        async pull(controller) {
                            if (closedOrErrored) return;
                            if (reading) return;
                            reading = true;
                            
                            const { done, value } = await sourceReader.read();
                            reading = false;
                            
                            if (done) {
                                closedOrErrored = true;
                                controller.close();
                            } else {
                                controller.enqueue(value);
                            }
                        }
                    });

                    const stream2 = new ReadableStream({
                        async pull(controller) {
                            if (closedOrErrored) return;
                            if (reading) return;
                            reading = true;
                            
                            const { done, value } = await sourceReader.read();
                            reading = false;
                            
                            if (done) {
                                closedOrErrored = true;
                                controller.close();
                            } else {
                                controller.enqueue(value);
                            }
                        }
                    });

                    // Read from stream2 only (like the failing test)
                    const reader2 = stream2.getReader();
                    let text = '';
                    while (true) {
                        const { done, value } = await reader2.read();
                        if (done) break;
                        text += new TextDecoder().decode(value);
                    }

                    event.respondWith(new Response(JSON.stringify({
                        success: true,
                        text
                    })));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        stack: error.stack
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

        let (task, rx) = Event::fetch(request);
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Two streams sharing reader should work"
        );
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        assert!(
            body_str.contains("AB") || (body_str.contains("A") && body_str.contains("B")),
            "Should contain data: {}",
            body_str
        );
    })
    .await;
}

/// Inline tee logic test - exactly what tee() does
#[tokio::test]
async fn test_inline_tee_logic() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    // Source stream
                    const source = new ReadableStream({
                        start(controller) {
                            controller.enqueue(new TextEncoder().encode('Test'));
                            controller.close();
                        }
                    });

                    // Exactly replicate tee() logic
                    const reader = source.getReader();
                    let closedOrErrored = false;
                    let reading = false;

                    function pullBoth(controller1, controller2) {
                        if (reading || closedOrErrored) {
                            return Promise.resolve();
                        }
                        reading = true;

                        return reader.read().then(({ done, value }) => {
                            reading = false;
                            if (done) {
                                closedOrErrored = true;
                                try { controller1.close(); } catch (e) {}
                                try { controller2.close(); } catch (e) {}
                                return;
                            }
                            controller1.enqueue(value);
                            controller2.enqueue(new Uint8Array(value));
                        });
                    }

                    let ctrl1 = null;
                    let ctrl2 = null;

                    const branch1 = new ReadableStream({
                        start(controller) { ctrl1 = controller; },
                        pull(controller) { return pullBoth(controller, ctrl2); }
                    });

                    const branch2 = new ReadableStream({
                        start(controller) { ctrl2 = controller; },
                        pull(controller) { return pullBoth(ctrl1, controller); }
                    });

                    // Read from branch2 only
                    const reader2 = branch2.getReader();
                    let text = '';
                    while (true) {
                        const { done, value } = await reader2.read();
                        if (done) break;
                        text += new TextDecoder().decode(value);
                    }

                    event.respondWith(new Response(JSON.stringify({
                        success: true,
                        text
                    })));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        stack: error.stack
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

        let (task, rx) = Event::fetch(request);
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(exec_result.is_ok(), "Inline tee logic should work");
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        assert!(
            body_str.contains("Test"),
            "Should contain data: {}",
            body_str
        );
    })
    .await;
}

/// Debug test to see where tee hangs
#[tokio::test]
async fn test_tee_debug() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                try {
                    const logs = [];
                    
                    const stream = new ReadableStream({
                        start(controller) {
                            logs.push('source start');
                            controller.enqueue(new TextEncoder().encode('Test'));
                            controller.close();
                            logs.push('source closed');
                        }
                    });

                    logs.push('before tee');
                    const [branch1, branch2] = stream.tee();
                    logs.push('after tee');

                    logs.push('before getReader');
                    const reader2 = branch2.getReader();
                    logs.push('after getReader');

                    logs.push('before read');
                    
                    // Use a timeout to detect hang
                    const readPromise = reader2.read();
                    const timeoutPromise = new Promise((_, reject) => {
                        setTimeout(() => reject(new Error('timeout')), 1000);
                    });

                    try {
                        const result = await Promise.race([readPromise, timeoutPromise]);
                        logs.push('after read: ' + JSON.stringify(result));
                    } catch (e) {
                        logs.push('read failed: ' + e.message);
                    }

                    event.respondWith(new Response(JSON.stringify({
                        logs
                    })));
                } catch (error) {
                    event.respondWith(new Response(JSON.stringify({
                        error: error.message,
                        stack: error.stack
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

        let (task, rx) = Event::fetch(request);
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        if exec_result.is_err() {
            panic!("Test timed out at worker.exec()");
        }
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        println!("Debug output: {}", body_str);

        assert!(body_str.contains("logs"), "Should have logs: {}", body_str);
    })
    .await;
}
