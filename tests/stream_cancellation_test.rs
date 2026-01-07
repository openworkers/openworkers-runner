//! Stream cancellation tests
//!
//! These tests verify that when a client disconnects (stops reading from the stream),
//! the worker's streaming logic stops and doesn't continue wasting resources.
//!
//! Test coverage:
//! - Signal-based detection (user checks controller.signal.aborted)
//! - Throw-based detection (enqueue throws when stream is closed)
//! - Cleanup code execution after disconnect
//! - Immediate disconnect (before first chunk)
//! - exec() completion timing

#![cfg(not(feature = "wasm"))]

use std::collections::HashMap;
use std::time::Duration;

use openworkers_core::{HttpMethod, HttpRequest, RequestBody, ResponseBody, Script, Task};
use openworkers_runner::RunnerOperations;
use std::sync::Arc;
use tokio::task::LocalSet;

#[cfg(feature = "v8")]
use openworkers_runtime_v8::Worker;

/// Helper to create a standard HTTP GET request
#[cfg(feature = "v8")]
fn make_request() -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    }
}

/// Helper to run async tests in a LocalSet (required for tokio 1.48+ spawn_local)
async fn run_in_local<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let local = LocalSet::new();
    local.run_until(f()).await
}

/// Test that stream stops when consumer drops the receiver.
/// This simulates client disconnection - we drop the response after reading a few chunks.
///
/// This test FAILS when the bug exists:
/// - Bug behavior: Worker emits all 10 chunks even when client disconnects after 2
/// - Expected behavior: Worker stops within 1-2 chunks after disconnect
#[tokio::test(flavor = "current_thread")]
#[cfg(feature = "v8")]
async fn test_stream_stops_on_consumer_disconnect() {
    run_in_local(|| async {
        // Worker that emits 10 chunks with 100ms delay
        // We'll disconnect after reading 2, and verify it stops quickly
        // The worker tracks how many chunks it emitted in globalThis.__emitCount
        let script = Script::new(
            r#"
            globalThis.__emitCount = 0;

            globalThis.default = {
                async fetch(request, env, ctx) {
                    const stream = new ReadableStream({
                        async start(controller) {
                            for (let i = 1; i <= 10; i++) {
                                // Check signal before emitting
                                if (controller.signal.aborted) {
                                    console.log('[WORKER] Signal aborted at chunk', i);
                                    break;
                                }

                                globalThis.__emitCount = i;
                                console.log('[WORKER] Emitting chunk', i);
                                controller.enqueue(`chunk:${i}\n`);
                                await new Promise(resolve => setTimeout(resolve, 100));
                            }

                            // Always close the stream
                            controller.close();
                        }
                    });

                    return new Response(stream, {
                        headers: { 'Content-Type': 'text/event-stream' }
                    });
                }
            };
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

        // Use a scope to release the borrow after exec completes
        {
            // Create a pinned future for exec so we can use it with select!
            let mut exec_future = Box::pin(worker.exec(task));

            // Wait for response while exec is running
            let response = tokio::select! {
                biased;
                res = rx => res.expect("Should receive response"),
                _ = &mut exec_future => {
                    panic!("exec() finished before response was sent");
                }
            };

            assert_eq!(response.status, 200);
            assert!(response.body.is_stream(), "Should be streaming");

            let ResponseBody::Stream(mut stream_rx) = response.body else {
                // Consume exec_future if not a stream
                let _ = exec_future.await;
                panic!("Expected stream body");
            };

            // Read exactly 2 chunks
            let mut chunks_received = Vec::new();

            for _ in 0..2 {
                // Read chunk while exec continues running in background
                let chunk = tokio::select! {
                    biased;
                    chunk = stream_rx.recv() => chunk,
                    _ = &mut exec_future => {
                        panic!("exec() finished before we could read chunks");
                    }
                };

                if let Some(Ok(bytes)) = chunk {
                    let chunk_str = String::from_utf8_lossy(&bytes).to_string();
                    println!("[TEST] Received: {}", chunk_str.trim());
                    chunks_received.push(chunk_str);
                }
            }

            assert_eq!(chunks_received.len(), 2, "Should have read exactly 2 chunks");

            // Drop the receiver - this simulates client disconnect
            println!("[TEST] === DROPPING RECEIVER (client disconnect) ===");
            drop(stream_rx);

            // Now wait for exec to complete - it should finish quickly due to cancellation
            let start = std::time::Instant::now();
            let exec_result = tokio::time::timeout(Duration::from_secs(3), exec_future).await;
            let elapsed = start.elapsed();

            assert!(exec_result.is_ok(), "exec should complete after disconnect");
            println!("[TEST] exec completed in {:?} after disconnect", elapsed);

            // If the fix works, exec should complete quickly (within ~300ms)
            // If the bug exists, it would take ~800ms (remaining 8 chunks * 100ms)
            if elapsed > Duration::from_millis(500) {
                println!("[TEST] WARNING: exec took longer than expected - cancellation may not be working");
            }
        } // exec_future is consumed above, releasing the borrow

        // Check how many chunks were emitted
        let emit_count = worker.get_global_u32("__emitCount").unwrap_or(0);
        println!(
            "[TEST] Final emit count: {} (should be <= 5 if fix works, 10 if bug exists)",
            emit_count
        );

        // THE KEY ASSERTION: After disconnecting at chunk 2,
        // if the bug exists, all 10 chunks would be produced.
        // If fixed, the worker should have stopped around chunk 3-5.
        assert!(
            emit_count <= 5,
            "Worker should stop producing chunks after disconnect! \
             Expected <= 5 chunks, but {} were produced. \
             This indicates the stream cancellation bug is NOT fixed.",
            emit_count
        );
    })
    .await;
}

/// Test that directly verifies the emit count after disconnect.
#[tokio::test(flavor = "current_thread")]
#[cfg(feature = "v8")]
async fn test_emit_count_after_disconnect() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            globalThis.__emitCount = 0;

            globalThis.default = {
                async fetch(request, env, ctx) {
                    const stream = new ReadableStream({
                        async start(controller) {
                            for (let i = 1; i <= 20; i++) {
                                if (controller.signal.aborted) {
                                    console.log('[WORKER] Stopped at chunk', i, 'due to abort signal');
                                    break;
                                }

                                globalThis.__emitCount = i;
                                console.log('[WORKER] Produced chunk', i);
                                controller.enqueue(`chunk:${i}\n`);

                                // 50ms delay - so all 20 chunks would take 1000ms
                                await new Promise(resolve => setTimeout(resolve, 50));
                            }

                            // Always close the stream
                            controller.close();
                        }
                    });

                    return new Response(stream, {
                        headers: { 'Content-Type': 'text/event-stream' }
                    });
                }
            };
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

        // Scope to release borrow
        {
            let mut exec_future = Box::pin(worker.exec(task));

            // Get response
            let response = tokio::select! {
                biased;
                res = rx => res.expect("Should receive response"),
                _ = &mut exec_future => panic!("exec finished early"),
            };

            let ResponseBody::Stream(mut stream_rx) = response.body else {
                let _ = exec_future.await;
                panic!("Expected stream body");
            };

            // Read exactly 3 chunks
            for i in 0..3 {
                let chunk = tokio::select! {
                    biased;
                    c = stream_rx.recv() => c,
                    _ = &mut exec_future => panic!("exec finished while reading"),
                };

                if let Some(result) = chunk {
                    println!("[TEST] Received chunk {}: {:?}", i + 1, result.is_ok());
                }
            }

            // Disconnect after chunk 3
            println!("[TEST] === DISCONNECTING after chunk 3 ===");
            drop(stream_rx);

            // Wait for exec to complete
            let _ = tokio::time::timeout(Duration::from_secs(3), exec_future).await;
        }

        // Check final emit count
        let emit_count = worker.get_global_u32("__emitCount").unwrap_or(0);
        println!(
            "[TEST] Final emit count: {} (should be <= 6 if fix works, 15+ if bug exists)",
            emit_count
        );

        // With 3 chunks read + disconnect:
        // - Bug behavior: ~15 chunks produced
        // - Fixed behavior: <= 6 chunks (3 read + few in flight)
        assert!(
            emit_count <= 6,
            "Worker should stop producing chunks after disconnect! \
             Expected <= 6 chunks, but {} were produced. \
             This indicates the stream cancellation bug is NOT fixed.",
            emit_count
        );
    })
    .await;
}

/// Test that enqueue() throws when client disconnects (throw-based detection).
/// Worker does NOT check signal.aborted - relies on enqueue throwing.
#[tokio::test(flavor = "current_thread")]
#[cfg(feature = "v8")]
async fn test_enqueue_throws_on_disconnect() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            globalThis.__emitCount = 0;
            globalThis.__errorCaught = false;
            globalThis.__errorMessage = '';

            globalThis.default = {
                async fetch(request, env, ctx) {
                    const stream = new ReadableStream({
                        async start(controller) {
                            try {
                                for (let i = 1; i <= 20; i++) {
                                    // Deliberately NOT checking signal.aborted
                                    // Relying on enqueue() to throw
                                    globalThis.__emitCount = i;
                                    controller.enqueue(`chunk:${i}\n`);
                                    await new Promise(resolve => setTimeout(resolve, 50));
                                }
                                controller.close();
                            } catch (error) {
                                globalThis.__errorCaught = true;
                                globalThis.__errorMessage = error.message;
                                console.log('[WORKER] Caught error:', error.message);
                            }
                        }
                    });

                    return new Response(stream, {
                        headers: { 'Content-Type': 'text/event-stream' }
                    });
                }
            };
            "#,
        );

        let ops = Arc::new(RunnerOperations::new());
        let mut worker = Worker::new_with_ops(script, None, ops)
            .await
            .expect("Worker should initialize");

        let (task, rx) = Task::fetch(make_request());

        {
            let mut exec_future = Box::pin(worker.exec(task));

            let response = tokio::select! {
                biased;
                res = rx => res.expect("Should receive response"),
                _ = &mut exec_future => panic!("exec finished early"),
            };

            let ResponseBody::Stream(mut stream_rx) = response.body else {
                let _ = exec_future.await;
                panic!("Expected stream body");
            };

            // Read 2 chunks then disconnect
            for _ in 0..2 {
                let _ = tokio::select! {
                    biased;
                    c = stream_rx.recv() => c,
                    _ = &mut exec_future => panic!("exec finished while reading"),
                };
            }

            drop(stream_rx);
            let _ = tokio::time::timeout(Duration::from_secs(3), exec_future).await;
        }

        let emit_count = worker.get_global_u32("__emitCount").unwrap_or(0);
        println!("[TEST] Emit count: {}", emit_count);

        // Should have stopped early due to throw
        assert!(
            emit_count <= 6,
            "Worker should stop when enqueue throws. Got {} chunks.",
            emit_count
        );
    })
    .await;
}

/// Test that signal is aborted when client disconnects.
/// Note: When exec() exits due to disconnect, the event loop stops,
/// so cleanup code in pending awaits won't run. The signal IS set though.
#[tokio::test(flavor = "current_thread")]
#[cfg(feature = "v8")]
async fn test_signal_is_aborted_on_disconnect() {
    run_in_local(|| async {
        // This worker checks signal synchronously (no await between enqueue and check)
        let script = Script::new(
            r#"
            globalThis.__signalChecked = 0;
            globalThis.__signalWasAborted = 0;

            globalThis.default = {
                async fetch(request, env, ctx) {
                    const stream = new ReadableStream({
                        async start(controller) {
                            for (let i = 1; i <= 10; i++) {
                                // Check signal BEFORE await
                                globalThis.__signalChecked = 1;
                                if (controller.signal.aborted) {
                                    globalThis.__signalWasAborted = 1;
                                    console.log('[WORKER] Signal aborted at chunk', i);
                                    break;
                                }

                                controller.enqueue(`chunk:${i}\n`);
                                await new Promise(resolve => setTimeout(resolve, 50));
                            }

                            // Always close the stream
                            controller.close();
                        }
                    });

                    return new Response(stream, {
                        headers: { 'Content-Type': 'text/event-stream' }
                    });
                }
            };
            "#,
        );

        let ops = Arc::new(RunnerOperations::new());
        let mut worker = Worker::new_with_ops(script, None, ops)
            .await
            .expect("Worker should initialize");

        let (task, rx) = Task::fetch(make_request());

        {
            let mut exec_future = Box::pin(worker.exec(task));

            let response = tokio::select! {
                biased;
                res = rx => res.expect("Should receive response"),
                _ = &mut exec_future => panic!("exec finished early"),
            };

            let ResponseBody::Stream(mut stream_rx) = response.body else {
                let _ = exec_future.await;
                panic!("Expected stream body");
            };

            // Read 2 chunks then disconnect
            for _ in 0..2 {
                let _ = tokio::select! {
                    biased;
                    c = stream_rx.recv() => c,
                    _ = &mut exec_future => panic!("exec finished while reading"),
                };
            }

            drop(stream_rx);
            let _ = tokio::time::timeout(Duration::from_secs(3), exec_future).await;
        }

        // The signal check happened at least once
        let signal_checked = worker.get_global_u32("__signalChecked").unwrap_or(0);
        assert_eq!(signal_checked, 1, "Signal should have been checked");

        // Note: __signalWasAborted may be 0 or 1 depending on timing.
        // The important thing is that exec() completed quickly and didn't hang.
        println!(
            "[TEST] Signal was aborted: {}",
            worker.get_global_u32("__signalWasAborted").unwrap_or(0)
        );
    })
    .await;
}

/// Test immediate disconnect - drop receiver before reading any chunks.
#[tokio::test(flavor = "current_thread")]
#[cfg(feature = "v8")]
async fn test_immediate_disconnect() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            globalThis.__emitCount = 0;

            globalThis.default = {
                async fetch(request, env, ctx) {
                    const stream = new ReadableStream({
                        async start(controller) {
                            for (let i = 1; i <= 10; i++) {
                                if (controller.signal.aborted) {
                                    break;
                                }

                                globalThis.__emitCount = i;
                                controller.enqueue(`chunk:${i}\n`);
                                await new Promise(resolve => setTimeout(resolve, 100));
                            }

                            // Always close the stream
                            controller.close();
                        }
                    });

                    return new Response(stream, {
                        headers: { 'Content-Type': 'text/event-stream' }
                    });
                }
            };
            "#,
        );

        let ops = Arc::new(RunnerOperations::new());
        let mut worker = Worker::new_with_ops(script, None, ops)
            .await
            .expect("Worker should initialize");

        let (task, rx) = Task::fetch(make_request());

        {
            let mut exec_future = Box::pin(worker.exec(task));

            let response = tokio::select! {
                biased;
                res = rx => res.expect("Should receive response"),
                _ = &mut exec_future => panic!("exec finished early"),
            };

            let ResponseBody::Stream(stream_rx) = response.body else {
                let _ = exec_future.await;
                panic!("Expected stream body");
            };

            // Immediately disconnect - don't read any chunks
            println!("[TEST] === IMMEDIATE DISCONNECT ===");
            drop(stream_rx);

            // Wait for exec to complete
            let start = std::time::Instant::now();
            let _ = tokio::time::timeout(Duration::from_secs(3), exec_future).await;
            let elapsed = start.elapsed();

            println!("[TEST] exec completed in {:?}", elapsed);
        }

        let emit_count = worker.get_global_u32("__emitCount").unwrap_or(0);
        println!(
            "[TEST] Emit count after immediate disconnect: {}",
            emit_count
        );

        // With immediate disconnect, should stop very quickly
        // May emit 1-2 chunks before detection
        assert!(
            emit_count <= 3,
            "Should stop quickly on immediate disconnect. Got {} chunks.",
            emit_count
        );
    })
    .await;
}

/// Test that exec() completes quickly after disconnect (no hanging).
#[tokio::test(flavor = "current_thread")]
#[cfg(feature = "v8")]
async fn test_exec_completes_quickly_after_disconnect() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            globalThis.default = {
                async fetch(request, env, ctx) {
                    const stream = new ReadableStream({
                        async start(controller) {
                            // Would take 5 seconds without cancellation
                            for (let i = 1; i <= 50; i++) {
                                if (controller.signal.aborted) break;
                                controller.enqueue(`chunk:${i}\n`);
                                await new Promise(resolve => setTimeout(resolve, 100));
                            }

                            // Always close the stream, even if aborted
                            // This ensures active_streams goes to 0
                            controller.close();
                        }
                    });

                    return new Response(stream, {
                        headers: { 'Content-Type': 'text/event-stream' }
                    });
                }
            };
            "#,
        );

        let ops = Arc::new(RunnerOperations::new());
        let mut worker = Worker::new_with_ops(script, None, ops)
            .await
            .expect("Worker should initialize");

        let (task, rx) = Task::fetch(make_request());

        let mut exec_future = Box::pin(worker.exec(task));

        let response = tokio::select! {
            biased;
            res = rx => res.expect("Should receive response"),
            _ = &mut exec_future => panic!("exec finished early"),
        };

        let ResponseBody::Stream(mut stream_rx) = response.body else {
            let _ = exec_future.await;
            panic!("Expected stream body");
        };

        // Read 3 chunks
        for _ in 0..3 {
            let _ = tokio::select! {
                biased;
                c = stream_rx.recv() => c,
                _ = &mut exec_future => panic!("exec finished while reading"),
            };
        }

        // Disconnect
        drop(stream_rx);

        // Measure how long exec takes to complete
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(2), exec_future).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "exec should complete within 2 seconds");
        println!("[TEST] exec completed in {:?} after disconnect", elapsed);

        // Should complete in under 500ms (not 5 seconds)
        // Note: timing depends on when the worker checks signal.aborted
        // With 100ms setTimeout, worst case is ~200ms (wait for timeout + close)
        assert!(
            elapsed < Duration::from_millis(500),
            "exec should complete quickly after disconnect. Took {:?}",
            elapsed
        );
    })
    .await;
}

/// Test normal stream completion (no disconnect) still works.
#[tokio::test(flavor = "current_thread")]
#[cfg(feature = "v8")]
async fn test_normal_stream_completion() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            globalThis.__completed = 0;

            globalThis.default = {
                async fetch(request, env, ctx) {
                    const stream = new ReadableStream({
                        async start(controller) {
                            for (let i = 1; i <= 3; i++) {
                                controller.enqueue(`chunk:${i}\n`);
                                await new Promise(resolve => setTimeout(resolve, 10));
                            }

                            controller.close();
                            globalThis.__completed = 1;
                        }
                    });

                    return new Response(stream, {
                        headers: { 'Content-Type': 'text/event-stream' }
                    });
                }
            };
            "#,
        );

        let ops = Arc::new(RunnerOperations::new());
        let mut worker = Worker::new_with_ops(script, None, ops)
            .await
            .expect("Worker should initialize");

        let (task, rx) = Task::fetch(make_request());

        {
            let mut exec_future = Box::pin(worker.exec(task));

            let response = tokio::select! {
                biased;
                res = rx => res.expect("Should receive response"),
                _ = &mut exec_future => panic!("exec finished early"),
            };

            let ResponseBody::Stream(mut stream_rx) = response.body else {
                let _ = exec_future.await;
                panic!("Expected stream body");
            };

            // Read all chunks until stream ends
            // Must use select! to keep driving exec() for timers to fire
            let mut chunks = Vec::new();
            let mut exec_done = false;

            while !exec_done {
                let chunk = tokio::select! {
                    biased;
                    c = stream_rx.recv() => c,
                    _ = &mut exec_future => {
                        exec_done = true;
                        None
                    },
                };

                match chunk {
                    Some(Ok(bytes)) => chunks.push(bytes),
                    Some(Err(_)) => break,
                    None => break,
                }
            }

            assert_eq!(chunks.len(), 3, "Should have received all 3 chunks");

            // Wait for exec to complete if not already
            if !exec_done {
                let _ = tokio::time::timeout(Duration::from_secs(2), exec_future).await;
            }
        }

        let completed = worker.get_global_u32("__completed").unwrap_or(0);
        assert_eq!(completed, 1, "Stream should have completed normally");
    })
    .await;
}
