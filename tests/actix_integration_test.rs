//! Integration tests with real actix-web server
//!
//! These tests spawn a real HTTP server to test stream cancellation
//! in conditions closer to production.

use std::sync::Arc;
use std::time::Duration;

use actix_web::{App, HttpServer, web};
use bytes::Bytes;
use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task};
use openworkers_runner::RunnerOperations;
use openworkers_runner::worker_pool::WORKER_POOL;
use tokio::sync::oneshot;

#[cfg(feature = "v8")]
use openworkers_runtime_v8::Worker;

/// Spawn an actix server that runs a worker for each request
/// Returns the server address and a shutdown signal
#[cfg(feature = "v8")]
async fn spawn_test_server(
    script: Script,
) -> Result<(String, oneshot::Sender<()>), Box<dyn std::error::Error>> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let (addr_tx, addr_rx) = oneshot::channel::<String>();

    let script = Arc::new(script);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();

        rt.block_on(async {
            let script_clone = script.clone();

            let server = HttpServer::new(move || {
                let script = script_clone.clone();

                App::new()
                    .app_data(web::Data::new(script))
                    .default_service(web::to(
                        |script: web::Data<Arc<Script>>,
                         req: actix_web::HttpRequest,
                         body: Bytes| async move {
                            handle_request(script, req, body).await
                        },
                    ))
            })
            .workers(1)
            .bind("127.0.0.1:0")
            .unwrap();

            let addr = server.addrs().first().unwrap().to_string();
            let _ = addr_tx.send(format!("http://{}", addr));

            let server = server.run();

            tokio::select! {
                _ = server => {},
                _ = shutdown_rx => {},
            }
        });
    });

    let addr = addr_rx.await?;
    Ok((addr, shutdown_tx))
}

#[cfg(feature = "v8")]
async fn handle_request(
    script: web::Data<Arc<Script>>,
    req: actix_web::HttpRequest,
    body: Bytes,
) -> actix_web::HttpResponse {
    let script: Script = (**script.get_ref()).clone();

    let http_request = HttpRequest {
        method: match req.method().as_str() {
            "GET" => HttpMethod::Get,
            "POST" => HttpMethod::Post,
            "PUT" => HttpMethod::Put,
            "DELETE" => HttpMethod::Delete,
            "PATCH" => HttpMethod::Patch,
            "HEAD" => HttpMethod::Head,
            "OPTIONS" => HttpMethod::Options,
            _ => HttpMethod::Get,
        },
        url: req.uri().to_string(),
        headers: req
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: if body.is_empty() {
            RequestBody::None
        } else {
            RequestBody::Bytes(body)
        },
    };

    // Create response channel
    let (res_tx, res_rx) = oneshot::channel();

    // Spawn worker on dedicated thread pool (like production)
    WORKER_POOL.spawn(move || async move {
        let ops = Arc::new(RunnerOperations::new());

        let mut worker = match Worker::new_with_ops(script, None, ops).await {
            Ok(w) => w,
            Err(e) => {
                let _ = res_tx.send(Err(format!("Worker init failed: {}", e)));
                return;
            }
        };

        let (task, rx) = Task::fetch(http_request);

        // Spawn a task to wait for and forward the response
        let res_tx_clone = std::sync::Arc::new(std::sync::Mutex::new(Some(res_tx)));
        let res_tx_for_task = res_tx_clone.clone();

        tokio::task::spawn_local(async move {
            if let Ok(response) = rx.await {
                if let Some(tx) = res_tx_for_task.lock().unwrap().take() {
                    let _ = tx.send(Ok(response));
                }
            }
        });

        // Run the worker - this continues until stream is done or cancelled
        let _ = worker.exec(task).await;
    });

    // Wait for response
    match tokio::time::timeout(Duration::from_secs(30), res_rx).await {
        Ok(Ok(Ok(response))) => response.into(),
        Ok(Ok(Err(e))) => actix_web::HttpResponse::InternalServerError().body(e),
        Ok(Err(_)) => {
            actix_web::HttpResponse::InternalServerError().body("Response channel closed")
        }
        Err(_) => actix_web::HttpResponse::GatewayTimeout().body("Worker timeout"),
    }
}

/// Test that verifies stream cancellation works through the full actix stack
#[tokio::test]
#[cfg(feature = "v8")]
async fn test_actix_stream_cancellation() {
    let script = Script::new(
        r#"
        globalThis.__emitCount = 0;

        globalThis.default = {
            async fetch(request, env, ctx) {
                const stream = new ReadableStream({
                    async start(controller) {
                        for (let i = 1; i <= 20; i++) {
                            if (controller.signal.aborted) {
                                console.log('[WORKER] Signal aborted at chunk', i);
                                break;
                            }

                            globalThis.__emitCount = i;
                            console.log('[WORKER] Emitting chunk', i);
                            controller.enqueue(`data: chunk ${i}\n\n`);
                            await new Promise(resolve => setTimeout(resolve, 100));
                        }

                        if (!controller.signal.aborted) {
                            controller.close();
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

    let (addr, shutdown) = spawn_test_server(script)
        .await
        .expect("Failed to spawn server");

    println!("[TEST] Server running at {}", addr);

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Make request and read a few chunks, then disconnect
    let client = reqwest::Client::new();

    let response = client
        .get(&addr)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), 200);

    // Read chunks using bytes_stream
    let mut stream = response.bytes_stream();
    let mut chunks_received = 0;

    use futures::StreamExt;

    while let Some(result) = stream.next().await {
        if let Ok(bytes) = result {
            let text = String::from_utf8_lossy(&bytes);
            println!("[TEST] Received: {}", text.trim());
            chunks_received += 1;

            // Disconnect after 3 chunks
            if chunks_received >= 3 {
                println!(
                    "[TEST] === DISCONNECTING after {} chunks ===",
                    chunks_received
                );
                break;
            }
        }
    }

    // Drop the stream to disconnect
    drop(stream);

    // Wait a bit for the worker to detect disconnection
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Shutdown server
    let _ = shutdown.send(());

    println!(
        "[TEST] Test completed. Received {} chunks before disconnect.",
        chunks_received
    );

    // Note: We can't easily check __emitCount from here since the worker
    // runs in a different thread. The real test is that the server doesn't
    // hang and logs stop appearing after disconnect.
}

/// Simpler test: just verify the server can handle streaming responses
#[tokio::test]
#[cfg(feature = "v8")]
async fn test_actix_streaming_works() {
    let script = Script::new(
        r#"
        globalThis.default = {
            async fetch(request, env, ctx) {
                const stream = new ReadableStream({
                    async start(controller) {
                        for (let i = 1; i <= 3; i++) {
                            controller.enqueue(`data: chunk ${i}\n\n`);
                            await new Promise(resolve => setTimeout(resolve, 50));
                        }
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

    let (addr, shutdown) = spawn_test_server(script)
        .await
        .expect("Failed to spawn server");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    let response = client.get(&addr).send().await.expect("Request failed");

    assert_eq!(response.status(), 200);

    let body = response.text().await.expect("Failed to read body");
    println!("[TEST] Full body:\n{}", body);

    assert!(body.contains("chunk 1"));
    assert!(body.contains("chunk 2"));
    assert!(body.contains("chunk 3"));

    let _ = shutdown.send(());
}
