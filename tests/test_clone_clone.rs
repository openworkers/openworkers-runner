//! Minimal test to reproduce the clone(clone()) bug

#![cfg(not(feature = "wasm"))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task};
use openworkers_runner::RunnerOperations;
use tokio::task::LocalSet;

#[cfg(feature = "v8")]
use openworkers_runtime_v8::Worker;

async fn run_in_local<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let local = LocalSet::new();
    local.run_until(f()).await
}

/// Minimal reproduction: clone a cached (already cloned) Response
#[tokio::test(flavor = "current_thread")]
async fn test_minimal_clone_clone() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', async (event) => {
                console.log("1. Fetching...");
                const response = await fetch('https://httpbin.org/get');
                
                console.log("2. First clone...");
                const clone1 = response.clone();
                
                console.log("3. Reading first clone...");
                const text1 = await clone1.text();
                console.log("4. First clone read OK, length:", text1.length);
                
                console.log("5. Second clone (cloning the already-cloned response)...");
                const clone2 = response.clone();
                
                console.log("6. Reading second clone...");
                const text2 = await clone2.text();
                console.log("7. Second clone read OK, length:", text2.length);
                
                event.respondWith(new Response(JSON.stringify({
                    success: true,
                    len1: text1.length,
                    len2: text2.length
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

        println!("Starting worker execution...");
        let exec_result = tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;

        assert!(
            exec_result.is_ok(),
            "Execution should complete within 10s timeout"
        );
        exec_result.unwrap().expect("Task should execute");

        let response = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("Response timeout")
            .expect("Should receive response");

        assert_eq!(response.status, 200);
        println!("Test PASSED!");
    })
    .await;
}

/// Test with multiple concurrent clone operations
#[tokio::test(flavor = "current_thread")]
async fn test_concurrent_clones() {
    let mut handles = vec![];

    for i in 0..5 {
        let handle = tokio::spawn(async move {
            run_in_local(|| async move {
                let script = Script::new(
                    r#"
                    addEventListener('fetch', async (event) => {
                        const response = await fetch('https://httpbin.org/get');
                        const clone1 = response.clone();
                        const text1 = await clone1.text();
                        const clone2 = response.clone();
                        const text2 = await clone2.text();
                        event.respondWith(new Response('OK'));
                    });
                    "#,
                );

                let ops = Arc::new(RunnerOperations::new());
                let mut worker = Worker::new_with_ops(script, None, ops)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: format!("http://localhost/{}", i),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);

                let exec_result =
                    tokio::time::timeout(Duration::from_secs(5), worker.exec(task)).await;
                assert!(exec_result.is_ok(), "Worker {} timed out", i);
                exec_result.unwrap().expect("Task should execute");

                let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
                println!("Worker {} completed", i);
            })
            .await
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.await.expect("Worker should complete");
    }

    println!("All workers completed!");
}
