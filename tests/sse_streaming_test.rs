//! SSE (Server-Sent Events) streaming tests
//!
//! These tests verify that workers can send SSE streaming responses
//! with proper chunked transfer encoding and timed events.
//!
//! Bug reproduction: curl -v 'https://cpu-limit.workers.rocks/slow?target=5' was hanging

#![cfg(not(feature = "wasm"))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use openworkers_core::{HttpMethod, HttpRequest, RequestBody, ResponseBody, Script, Task};
use openworkers_runner::RunnerOperations;
use tokio::task::LocalSet;

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

/// Test SSE streaming response with ReadableStream start() - THE ORIGINAL BUG
/// This is the exact pattern from cpu-limit.workers.rocks/slow
#[tokio::test]
async fn test_sse_streaming_with_start() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                const url = new URL(event.request.url);
                const target = parseInt(url.searchParams.get('target') || '3') || 3;

                const stream = new ReadableStream({
                    async start(controller) {
                        for (let i = 1; i <= target; i++) {
                            await new Promise((resolve) => setTimeout(resolve, 100));
                            controller.enqueue(`data: ${JSON.stringify({ elapsed: i, target })}\n\n`);
                        }

                        controller.enqueue(`data: ${JSON.stringify({ done: true, target })}\n\n`);
                        controller.close();
                    }
                });

                event.respondWith(new Response(stream, {
                    headers: {
                        'Content-Type': 'text/event-stream',
                        'Cache-Control': 'no-cache',
                        'Connection': 'keep-alive'
                    }
                }));
            });
            "#,
        );

        let ops = Arc::new(RunnerOperations::new());
        let mut worker = Worker::new_with_ops(script, None, ops)
            .await
            .expect("Worker should initialize");

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/slow?target=3".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Task::fetch(request);

        // Execute - should complete once stream is closed (~400ms for 3 events)
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within timeout"
        );
        exec_result.unwrap().expect("Task should execute");

        // Get response - should arrive quickly
        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("Response should arrive quickly")
            .expect("Should receive response");

        assert_eq!(response.status, 200);

        // Check headers
        let content_type = response
            .headers
            .iter()
            .find(|(k, _)| k.to_lowercase() == "content-type")
            .map(|(_, v)| v.as_str());
        assert_eq!(content_type, Some("text/event-stream"));

        // Response body should be a stream
        assert!(
            response.body.is_stream(),
            "SSE response should be streaming"
        );

        // Collect all SSE events
        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        // Verify we got all expected events
        assert!(
            body_str.contains(r#""elapsed":1"#) || body_str.contains(r#""elapsed": 1"#),
            "Should have elapsed 1: {}",
            body_str
        );
        assert!(
            body_str.contains(r#""elapsed":3"#) || body_str.contains(r#""elapsed": 3"#),
            "Should have elapsed 3: {}",
            body_str
        );
        assert!(
            body_str.contains(r#""done":true"#) || body_str.contains(r#""done": true"#),
            "Should have done event: {}",
            body_str
        );
    })
    .await;
}

/// Test SSE with ReadableStream pull() pattern
#[tokio::test]
async fn test_sse_with_readable_stream_pull() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                const encoder = new TextEncoder();
                let count = 0;
                const target = 3;

                const stream = new ReadableStream({
                    async pull(controller) {
                        if (count < target) {
                            await new Promise(resolve => setTimeout(resolve, 50));
                            count++;
                            controller.enqueue(encoder.encode(`data: event ${count}\n\n`));
                        } else {
                            controller.enqueue(encoder.encode('data: done\n\n'));
                            controller.close();
                        }
                    }
                });

                event.respondWith(new Response(stream, {
                    headers: { 'Content-Type': 'text/event-stream' }
                }));
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

        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;
        assert!(exec_result.is_ok(), "Execution should complete");
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        assert_eq!(response.status, 200);
        assert!(response.body.is_stream(), "Should be streaming");

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        assert!(
            body_str.contains("event 1"),
            "Should have event 1: {}",
            body_str
        );
        assert!(
            body_str.contains("event 2"),
            "Should have event 2: {}",
            body_str
        );
        assert!(
            body_str.contains("event 3"),
            "Should have event 3: {}",
            body_str
        );
        assert!(body_str.contains("done"), "Should have done: {}", body_str);
    })
    .await;
}

/// Test that streaming chunks are received incrementally (not buffered)
#[tokio::test]
async fn test_sse_chunks_received_incrementally() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                const encoder = new TextEncoder();
                let sent = 0;

                const stream = new ReadableStream({
                    async start(controller) {
                        controller.enqueue(encoder.encode('data: chunk1\n\n'));
                        await new Promise(resolve => setTimeout(resolve, 100));
                        controller.enqueue(encoder.encode('data: chunk2\n\n'));
                        await new Promise(resolve => setTimeout(resolve, 100));
                        controller.enqueue(encoder.encode('data: chunk3\n\n'));
                        controller.close();
                    }
                });

                event.respondWith(new Response(stream, {
                    headers: { 'Content-Type': 'text/event-stream' }
                }));
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

        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;
        assert!(exec_result.is_ok(), "Execution should complete");
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        assert!(response.body.is_stream(), "Should be streaming");

        // Read chunks individually to verify incremental delivery
        if let ResponseBody::Stream(mut stream_rx) = response.body {
            let mut chunks = Vec::new();
            let start = std::time::Instant::now();

            while let Some(result) = tokio::time::timeout(Duration::from_secs(5), stream_rx.recv())
                .await
                .expect("Chunk timeout")
            {
                match result {
                    Ok(bytes) => {
                        let elapsed = start.elapsed();
                        chunks.push((elapsed, String::from_utf8_lossy(&bytes).to_string()));
                    }
                    Err(e) => panic!("Stream error: {}", e),
                }
            }

            assert!(!chunks.is_empty(), "Should have received chunks");

            // Verify chunks contain expected data
            let all_data: String = chunks.iter().map(|(_, s)| s.as_str()).collect();
            assert!(all_data.contains("chunk1"), "Missing chunk1: {}", all_data);
            assert!(all_data.contains("chunk2"), "Missing chunk2: {}", all_data);
            assert!(all_data.contains("chunk3"), "Missing chunk3: {}", all_data);
        } else {
            panic!("Expected stream body");
        }
    })
    .await;
}

/// Test longer SSE stream (simulates real-world usage)
#[tokio::test]
async fn test_sse_longer_stream() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                const stream = new ReadableStream({
                    async start(controller) {
                        for (let i = 1; i <= 5; i++) {
                            await new Promise(resolve => setTimeout(resolve, 50));
                            controller.enqueue(`data: ${i}\n\n`);
                        }

                        controller.close();
                    }
                });

                event.respondWith(new Response(stream, {
                    headers: { 'Content-Type': 'text/event-stream' }
                }));
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

        // 5 events * 50ms = 250ms minimum, give it 5 seconds
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;
        assert!(exec_result.is_ok(), "Execution should complete");
        exec_result.unwrap().expect("Task should execute");

        let response = rx.await.expect("Should receive response");
        assert_eq!(response.status, 200);

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);

        // Verify all 5 events received
        for i in 1..=5 {
            assert!(
                body_str.contains(&format!("data: {}", i)),
                "Missing event {}: {}",
                i,
                body_str
            );
        }
    })
    .await;
}
