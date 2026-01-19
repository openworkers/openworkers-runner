//! Request body streaming tests
//!
//! Tests for streaming request bodies from Rust to JavaScript workers.
//! This is a critical feature that allows workers to process large uploads
//! without buffering the entire body in memory.

#![cfg(not(feature = "wasm"))]

use bytes::Bytes;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, ResponseBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep, timeout};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Helper to run async tests in a LocalSet with timeout
async fn run_local<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let local = tokio::task::LocalSet::new();
    timeout(TEST_TIMEOUT, local.run_until(f()))
        .await
        .expect("Test timed out after 5 seconds")
}

// =============================================================================
// BASIC TESTS - Verify streaming works at all
// =============================================================================

/// Test: Stream body consumed with .text()
/// Verifies that a streaming body can be read as text in JS
#[tokio::test]
async fn test_stream_body_as_text() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const text = await event.request.text();
                event.respondWith(new Response('Got: ' + text));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send chunks
        tokio::spawn(async move {
            tx.send(Ok(Bytes::from("Hello "))).await.unwrap();
            tx.send(Ok(Bytes::from("streaming "))).await.unwrap();
            tx.send(Ok(Bytes::from("world!"))).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            "Got: Hello streaming world!"
        );
    })
    .await;
}

/// Test: Stream body consumed with .json()
/// Verifies JSON parsing works on streamed bodies
#[tokio::test]
async fn test_stream_body_as_json() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const data = await event.request.json();
                event.respondWith(new Response('name=' + data.name + ',age=' + data.age));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send JSON in chunks (split mid-value)
        tokio::spawn(async move {
            tx.send(Ok(Bytes::from(r#"{"name":"#))).await.unwrap();
            tx.send(Ok(Bytes::from(r#""Alice","#))).await.unwrap();
            tx.send(Ok(Bytes::from(r#""age":30}"#))).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "name=Alice,age=30");
    })
    .await;
}

/// Test: Stream body consumed with .arrayBuffer()
/// Verifies binary data is correctly received
#[tokio::test]
async fn test_stream_body_as_arraybuffer() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const buffer = await event.request.arrayBuffer();
                const bytes = new Uint8Array(buffer);
                let sum = 0;
                for (let i = 0; i < bytes.length; i++) sum += bytes[i];
                event.respondWith(new Response('len=' + bytes.length + ',sum=' + sum));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send binary data: [1, 2, 3, 4, 5]
        tokio::spawn(async move {
            tx.send(Ok(Bytes::from(vec![1u8, 2, 3]))).await.unwrap();
            tx.send(Ok(Bytes::from(vec![4u8, 5]))).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "len=5,sum=15");
    })
    .await;
}

/// Test: Stream body read chunk by chunk with getReader()
/// Verifies manual stream consumption works
#[tokio::test]
async fn test_stream_body_with_reader() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();
                const chunks = [];

                while (true) {
                    const { done, value } = await reader.read();
                    if (done) break;
                    chunks.push(new TextDecoder().decode(value));
                }

                event.respondWith(new Response('chunks=' + chunks.length + ':' + chunks.join('|')));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        tokio::spawn(async move {
            tx.send(Ok(Bytes::from("one"))).await.unwrap();
            tx.send(Ok(Bytes::from("two"))).await.unwrap();
            tx.send(Ok(Bytes::from("three"))).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "chunks=3:one|two|three");
    })
    .await;
}

// =============================================================================
// PASSTHROUGH TESTS - Stream in, stream out
// =============================================================================

/// Test: Echo - stream body directly to response
/// Verifies passthrough works without consuming the body
#[tokio::test]
async fn test_stream_body_echo() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                // Pass request body directly to response
                event.respondWith(new Response(event.request.body));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        tokio::spawn(async move {
            for msg in ["Hello", " ", "World", "!"] {
                tx.send(Ok(Bytes::from(msg))).await.unwrap();
                sleep(Duration::from_millis(5)).await;
            }
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "Hello World!");
    })
    .await;
}

/// Test: Transform stream - read numbers, output doubled values
/// Verifies streaming transformation with matching chunk count
#[tokio::test]
async fn test_stream_body_transform() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();

                const stream = new ReadableStream({
                    async pull(controller) {
                        const { done, value } = await reader.read();
                        if (done) {
                            controller.close();
                            return;
                        }
                        const num = parseInt(new TextDecoder().decode(value));
                        controller.enqueue(new TextEncoder().encode((num * 2).toString()));
                    }
                });

                event.respondWith(new Response(stream));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send numbers 1, 2, 3, 4, 5
        tokio::spawn(async move {
            for n in 1..=5 {
                tx.send(Ok(Bytes::from(n.to_string()))).await.unwrap();
            }
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();

        // Verify streaming response
        let mut rx = match response.body {
            ResponseBody::Stream(rx) => rx,
            _ => panic!("Expected streaming response"),
        };

        let mut results = Vec::new();

        while let Some(chunk) = rx.recv().await {
            let bytes = chunk.unwrap();
            let text = String::from_utf8_lossy(&bytes);
            results.push(text.to_string());
        }

        // Should get 5 chunks with values 2, 4, 6, 8, 10
        assert_eq!(results.len(), 5, "Should have 5 output chunks");
        assert_eq!(results, vec!["2", "4", "6", "8", "10"]);
    })
    .await;
}

// =============================================================================
// EDGE CASES - Make sure we handle weird situations
// =============================================================================

/// Test: Empty stream (channel closes immediately)
#[tokio::test]
async fn test_stream_body_empty() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const text = await event.request.text();
                event.respondWith(new Response('length=' + text.length));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Close immediately - empty stream
        drop(tx);

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "length=0");
    })
    .await;
}

/// Test: Error mid-stream
#[tokio::test]
async fn test_stream_body_error() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                try {
                    const text = await event.request.text();
                    event.respondWith(new Response('Got: ' + text));
                } catch (e) {
                    event.respondWith(new Response('Error: ' + e.message, { status: 500 }));
                }
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        tokio::spawn(async move {
            tx.send(Ok(Bytes::from("partial"))).await.unwrap();
            tx.send(Err("Connection reset".to_string())).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body);

        // Should either get partial data or error message
        assert!(
            body_text.contains("partial") || body_text.contains("Error"),
            "Got: {}",
            body_text
        );
    })
    .await;
}

/// Test: JS cancels stream early
#[tokio::test]
async fn test_stream_body_cancelled() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();

                // Read only first chunk
                const { value } = await reader.read();
                const first = new TextDecoder().decode(value);

                // Cancel the rest
                await reader.cancel();

                event.respondWith(new Response('First: ' + first));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        tokio::spawn(async move {
            for i in 1..=10 {
                if tx
                    .send(Ok(Bytes::from(format!("chunk{}", i))))
                    .await
                    .is_err()
                {
                    break; // Channel closed
                }
                sleep(Duration::from_millis(10)).await;
            }
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "First: chunk1");
    })
    .await;
}

/// Test: Body not consumed at all
#[tokio::test]
async fn test_stream_body_ignored() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                // Don't read body at all
                event.respondWith(new Response('Ignored'));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        tokio::spawn(async move {
            for _ in 0..5 {
                if tx.send(Ok(Bytes::from("data"))).await.is_err() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "Ignored");
    })
    .await;
}

/// Test: Large streaming body (100 chunks x 1KB)
#[tokio::test]
async fn test_stream_body_large() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();
                let totalBytes = 0;
                let chunkCount = 0;

                while (true) {
                    const { done, value } = await reader.read();
                    if (done) break;
                    totalBytes += value.length;
                    chunkCount++;
                }

                event.respondWith(new Response('chunks=' + chunkCount + ',bytes=' + totalBytes));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        let num_chunks = 100;
        let chunk_size = 1024;

        tokio::spawn(async move {
            for _ in 0..num_chunks {
                let chunk = Bytes::from(vec![b'X'; chunk_size]);

                if tx.send(Ok(chunk)).await.is_err() {
                    break;
                }
            }
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        let expected = format!("chunks={},bytes={}", num_chunks, num_chunks * chunk_size);
        assert_eq!(String::from_utf8_lossy(&body), expected);
    })
    .await;
}

/// Test: UTF-8 character split across chunks
#[tokio::test]
async fn test_stream_body_utf8_boundary() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const text = await event.request.text();
                const charCount = [...text].length;
                event.respondWith(new Response('chars=' + charCount + ':' + text));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // 🎉 = F0 9F 8E 89 (4 bytes), split in the middle
        tokio::spawn(async move {
            // "Hi " + first 2 bytes of 🎉
            tx.send(Ok(Bytes::from(vec![b'H', b'i', b' ', 0xF0, 0x9F])))
                .await
                .unwrap();
            // Last 2 bytes of 🎉 + "!"
            tx.send(Ok(Bytes::from(vec![0x8E, 0x89, b'!'])))
                .await
                .unwrap();
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "chars=5:Hi 🎉!");
    })
    .await;
}

/// Test: Binary data with null bytes
#[tokio::test]
async fn test_stream_body_binary_nulls() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const buffer = await event.request.arrayBuffer();
                const bytes = new Uint8Array(buffer);
                let nulls = 0;
                for (let i = 0; i < bytes.length; i++) {
                    if (bytes[i] === 0) nulls++;
                }
                event.respondWith(new Response('len=' + bytes.length + ',nulls=' + nulls));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Binary with embedded nulls
        tokio::spawn(async move {
            tx.send(Ok(Bytes::from(vec![1, 0, 2, 0]))).await.unwrap();
            tx.send(Ok(Bytes::from(vec![0, 3, 0]))).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "len=7,nulls=4");
    })
    .await;
}

/// Test: Double consumption attempt (should fail)
#[tokio::test]
async fn test_stream_body_double_consume() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const text1 = await event.request.text();
                try {
                    const text2 = await event.request.text();
                    event.respondWith(new Response('ERROR: second read worked'));
                } catch (e) {
                    event.respondWith(new Response('OK: first=' + text1));
                }
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        tokio::spawn(async move {
            tx.send(Ok(Bytes::from("data"))).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.starts_with("OK:"),
            "Expected body used error, got: {}",
            text
        );
    })
    .await;
}

// =============================================================================
// BACKPRESSURE TESTS - Critical for memory safety
// =============================================================================

/// Test: Input backpressure - fast Rust producer, slow JS consumer
/// The channel should block when full, preventing memory explosion
#[tokio::test]
async fn test_backpressure_input_slow_consumer() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();
                let chunks = 0;
                let bytes = 0;

                while (true) {
                    const { done, value } = await reader.read();
                    if (done) break;

                    // Simulate slow consumer - 5ms per chunk
                    await new Promise(r => setTimeout(r, 5));

                    chunks++;
                    bytes += value.length;
                }

                event.respondWith(new Response('chunks=' + chunks + ',bytes=' + bytes));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        // Small buffer (4) to test backpressure kicks in
        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(4);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Fast producer - 10 chunks, should block when buffer is full
        let producer = tokio::spawn(async move {
            let mut sent = 0;

            for i in 0..10 {
                if tx
                    .send(Ok(Bytes::from(format!("chunk{:02}", i))))
                    .await
                    .is_err()
                {
                    break;
                }
                sent += 1;
            }

            sent
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let sent = producer.await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        let text = String::from_utf8_lossy(&body);

        // All 10 chunks should be sent and received
        assert_eq!(sent, 10);
        assert!(
            text.contains("chunks=10"),
            "Expected 10 chunks, got: {}",
            text
        );
    })
    .await;
}

/// Test: Output backpressure - fast JS producer, verify streaming response
/// JS produces chunks faster than we consume them
#[tokio::test]
async fn test_backpressure_output_fast_producer() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                let produced = 0;

                const stream = new ReadableStream({
                    async pull(controller) {
                        if (produced >= 10) {
                            controller.close();
                            return;
                        }

                        // Produce data quickly
                        const chunk = 'X'.repeat(100);
                        controller.enqueue(new TextEncoder().encode(chunk));
                        produced++;
                    }
                });

                event.respondWith(new Response(stream));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();

        let mut rx = match response.body {
            ResponseBody::Stream(rx) => rx,
            _ => panic!("Expected streaming response"),
        };

        // Slow consumer - simulate network latency
        let mut total_bytes = 0;
        let mut chunk_count = 0;

        while let Some(chunk) = rx.recv().await {
            let bytes = chunk.unwrap();
            total_bytes += bytes.len();
            chunk_count += 1;

            // Simulate slow consumer
            sleep(Duration::from_millis(2)).await;
        }

        assert_eq!(chunk_count, 10);
        assert_eq!(total_bytes, 10 * 100);
    })
    .await;
}

/// Test: Bidirectional backpressure - streaming in AND out simultaneously
/// This is the most complex case: request body streaming while response is also streaming
#[tokio::test]
async fn test_backpressure_bidirectional() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();
                let inputChunks = 0;

                const stream = new ReadableStream({
                    async pull(controller) {
                        const { done, value } = await reader.read();
                        if (done) {
                            controller.close();
                            return;
                        }

                        inputChunks++;

                        // Transform: parse number, multiply by 2
                        const num = parseInt(new TextDecoder().decode(value));
                        const result = (num * 2).toString();
                        controller.enqueue(new TextEncoder().encode(result));
                    }
                });

                event.respondWith(new Response(stream));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        // Small buffer for input
        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(2);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Producer sends numbers
        let producer = tokio::spawn(async move {
            for n in 1..=5 {
                if tx.send(Ok(Bytes::from(n.to_string()))).await.is_err() {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();

        let mut rx = match response.body {
            ResponseBody::Stream(rx) => rx,
            _ => panic!("Expected streaming response"),
        };

        // Consume response
        let mut results = Vec::new();

        while let Some(chunk) = rx.recv().await {
            let bytes = chunk.unwrap();
            let text = String::from_utf8_lossy(&bytes);
            results.push(text.parse::<i32>().unwrap());
            sleep(Duration::from_millis(2)).await;
        }

        producer.await.unwrap();

        // Should get 2, 4, 6, 8, 10
        let expected: Vec<i32> = (1..=5).map(|n| n * 2).collect();
        assert_eq!(results, expected);
    })
    .await;
}

/// Test: Backpressure with very small buffer (size 1)
/// Extreme case - only one chunk can be buffered at a time
#[tokio::test]
async fn test_backpressure_minimal_buffer() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();
                const chunks = [];

                while (true) {
                    const { done, value } = await reader.read();
                    if (done) break;
                    chunks.push(new TextDecoder().decode(value));
                    // Small delay to let producer try to fill buffer
                    await new Promise(r => setTimeout(r, 5));
                }

                event.respondWith(new Response('chunks=' + chunks.length));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        // Minimal buffer - only 1 element!
        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(1);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        let producer = tokio::spawn(async move {
            let mut sent = 0;

            for i in 0..10 {
                // This will block until consumer reads
                if tx.send(Ok(Bytes::from(format!("{}", i)))).await.is_err() {
                    break;
                }
                sent += 1;
            }

            sent
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let sent = producer.await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();

        assert_eq!(sent, 10);
        assert_eq!(String::from_utf8_lossy(&body), "chunks=10");
    })
    .await;
}

/// Test: Producer faster than consumer - verify no data loss
#[tokio::test]
async fn test_backpressure_no_data_loss() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();
                let sum = 0;
                let count = 0;

                while (true) {
                    const { done, value } = await reader.read();
                    if (done) break;

                    const num = parseInt(new TextDecoder().decode(value));
                    sum += num;
                    count++;

                    // Slow consumer - 2ms delay
                    await new Promise(r => setTimeout(r, 2));
                }

                event.respondWith(new Response('count=' + count + ',sum=' + sum));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(4);
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send numbers 1..=20, calculate expected sum
        let expected_sum: i32 = (1..=20).sum();

        let producer = tokio::spawn(async move {
            for n in 1..=20 {
                if tx.send(Ok(Bytes::from(n.to_string()))).await.is_err() {
                    break;
                }
                // Fast producer - no delay
            }
        });

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();
        producer.await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        let text = String::from_utf8_lossy(&body);

        // Verify no data loss: sum should be 210
        assert!(
            text.contains(&format!("sum={}", expected_sum)),
            "Expected sum={}, got: {}",
            expected_sum,
            text
        );
        assert!(
            text.contains("count=20"),
            "Expected count=20, got: {}",
            text
        );
    })
    .await;
}

// =============================================================================
// REGRESSION TESTS - Make sure we didn't break existing functionality
// =============================================================================

/// Test: RequestBody::Bytes still works (regression test for the take() bug)
#[tokio::test]
async fn test_bytes_body_still_works() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const text = await event.request.text();
                event.respondWith(new Response('Got: ' + text));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Bytes(Bytes::from("Hello from Bytes!")),
        };

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "Got: Hello from Bytes!");
    })
    .await;
}

/// Test: RequestBody::None still works
#[tokio::test]
async fn test_none_body_still_works() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const text = await event.request.text();
                event.respondWith(new Response('length=' + text.length));
            });"#,
        );

        let mut worker = Worker::new(script, None).await.unwrap();

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, response_rx) = Event::fetch(request);
        worker.exec(task).await.unwrap();

        let response = response_rx.await.unwrap();
        let body = response.body.collect().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "length=0");
    })
    .await;
}
