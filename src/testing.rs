//! Generic test utilities for OpenWorkers runtimes
//!
//! This module provides macros to generate standard tests for any runtime
//! that implements the `Worker` trait.
//!
//! # Usage
//!
//! In your runtime's test file:
//!
//! ```ignore
//! use openworkers_runner::generate_worker_tests;
//! use your_runtime::Worker;
//!
//! generate_worker_tests!(Worker);
//! ```

/// Generate standard worker tests for a runtime
///
/// This macro generates a comprehensive test suite for any Worker implementation.
/// Tests cover: basic responses, JSON, custom status, request properties, async handlers, etc.
#[macro_export]
macro_rules! generate_worker_tests {
    ($worker:ty) => {
        use openworkers_core::{
            HttpMethod, HttpRequest, RequestBody, ResponseBody, Script, Task, Worker as _,
        };
        use std::collections::HashMap;

        /// Helper to run async tests in a LocalSet (required for tokio 1.48+ spawn_local)
        async fn run_in_local<F, Fut, T>(f: F) -> T
        where
            F: FnOnce() -> Fut,
            Fut: std::future::Future<Output = T>,
        {
            let local = tokio::task::LocalSet::new();
            local.run_until(f()).await
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_simple_response() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        event.respondWith(new Response('Hello, World!'));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                assert_eq!(response.status, 200);

                let body = response.body.collect().await.expect("Should have body");
                assert_eq!(String::from_utf8_lossy(&body), "Hello, World!");
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_json_response() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        const data = { message: 'Hello', value: 42 };
                        event.respondWith(new Response(JSON.stringify(data), {
                            headers: { 'Content-Type': 'application/json' }
                        }));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                assert_eq!(response.status, 200);

                let content_type = response
                    .headers
                    .iter()
                    .find(|(k, _)| k.to_lowercase() == "content-type")
                    .map(|(_, v)| v.as_str());
                assert_eq!(content_type, Some("application/json"));

                let body = response.body.collect().await.expect("Should have body");
                let body_str = String::from_utf8_lossy(&body);
                assert!(
                    body_str.contains("Hello"),
                    "Body should contain 'Hello': {}",
                    body_str
                );
                assert!(
                    body_str.contains("42"),
                    "Body should contain '42': {}",
                    body_str
                );
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_custom_status() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        event.respondWith(new Response('Not Found', { status: 404 }));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                assert_eq!(response.status, 404);
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_request_method() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        const method = event.request.method;
                        event.respondWith(new Response(method));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Post,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                let body = response.body.collect().await.expect("Should have body");
                assert_eq!(String::from_utf8_lossy(&body), "POST");
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_request_url() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        const url = new URL(event.request.url);
                        event.respondWith(new Response(url.pathname));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/api/test".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                let body = response.body.collect().await.expect("Should have body");
                assert_eq!(String::from_utf8_lossy(&body), "/api/test");
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_async_handler() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        event.respondWith(
                            Promise.resolve({ async: true })
                                .then(data => new Response(JSON.stringify(data)))
                        );
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                let body = response.body.collect().await.expect("Should have body");
                let body_str = String::from_utf8_lossy(&body);
                assert!(
                    body_str.contains("true"),
                    "Body should contain async:true: {}",
                    body_str
                );
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_response_headers() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        event.respondWith(new Response('OK', {
                            status: 201,
                            headers: {
                                'X-Custom-Header': 'custom-value',
                                'X-Another': 'another-value'
                            }
                        }));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                assert_eq!(response.status, 201);

                let custom_header = response
                    .headers
                    .iter()
                    .find(|(k, _)| k.to_lowercase() == "x-custom-header")
                    .map(|(_, v)| v.as_str());
                assert_eq!(custom_header, Some("custom-value"));
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_empty_response() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        event.respondWith(new Response(null, { status: 204 }));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                assert_eq!(response.status, 204);
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_console_log() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        console.log('Log message');
                        event.respondWith(new Response('logged'));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                assert_eq!(response.status, 200);
            })
            .await;
        }

        /// Test that console methods (log, warn, error) work correctly
        /// Note: Log capture is handled by OperationsHandler, not via Worker constructor.
        /// This test verifies console methods don't crash and the worker completes.
        #[tokio::test(flavor = "current_thread")]
        async fn test_console_methods() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        console.log('info message');
                        console.warn('warn message');
                        console.error('error message');
                        console.debug('debug message');
                        console.trace('trace message');
                        event.respondWith(new Response('logged'));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                assert_eq!(response.status, 200);

                let body = response.body.collect().await.expect("Should have body");
                assert_eq!(String::from_utf8_lossy(&body), "logged");
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_worker_creation_error() {
            run_in_local(|| async {
                let script = r#"
                    this is not valid javascript
                "#;

                let script_obj = Script::new(script);
                let result = <$worker>::new(script_obj, None).await;
                assert!(
                    result.is_err(),
                    "Invalid script should fail to create worker"
                );
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_response_body_is_streamed() {
            run_in_local(|| async {
                // All responses with body should be streamed
                let script = r#"
                    addEventListener('fetch', (event) => {
                        event.respondWith(new Response('Hello, streaming!'));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                assert_eq!(response.status, 200);

                // All responses with body should be streamed
                assert!(
                    response.body.is_stream(),
                    "Response body should be a Stream"
                );

                // Collect and verify content
                let body = response.body.collect().await.expect("Should have body");
                assert_eq!(String::from_utf8_lossy(&body), "Hello, streaming!");
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_empty_response_body() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        event.respondWith(new Response(null, { status: 204 }));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                assert_eq!(response.status, 204);

                // Empty body should NOT be a stream
                assert!(
                    !response.body.is_stream(),
                    "Empty response body should not be a Stream"
                );
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_multiple_requests() {
            run_in_local(|| async {
                let script = r#"
                    let counter = 0;
                    addEventListener('fetch', (event) => {
                        counter++;
                        event.respondWith(new Response('Request ' + counter));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                for i in 1..=3 {
                    let request = HttpRequest {
                        method: HttpMethod::Get,
                        url: "http://localhost/".to_string(),
                        headers: HashMap::new(),
                        body: RequestBody::None,
                    };

                    let (task, rx) = Task::fetch(request);
                    worker.exec(task).await.expect("Task should execute");

                    let response = rx.await.expect("Should receive response");
                    let body = response.body.collect().await.expect("Should have body");
                    assert_eq!(String::from_utf8_lossy(&body), format!("Request {}", i));
                }
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_request_body_text() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', async (event) => {
                        const body = await event.request.text();
                        event.respondWith(new Response('Received: ' + body));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let request = HttpRequest {
                    method: HttpMethod::Post,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::Bytes(bytes::Bytes::from("Hello, World!")),
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                assert_eq!(response.status, 200);

                let body = response.body.collect().await.expect("Should have body");
                assert_eq!(String::from_utf8_lossy(&body), "Received: Hello, World!");
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_request_body_json() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', async (event) => {
                        const data = await event.request.json();
                        event.respondWith(new Response(JSON.stringify({
                            received: true,
                            name: data.name,
                            count: data.items.length
                        }), {
                            headers: { 'Content-Type': 'application/json' }
                        }));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let mut headers = HashMap::new();
                headers.insert("Content-Type".to_string(), "application/json".to_string());

                let request = HttpRequest {
                    method: HttpMethod::Post,
                    url: "http://localhost/".to_string(),
                    headers,
                    body: RequestBody::Bytes(bytes::Bytes::from(
                        r#"{"name":"test","items":[1,2,3]}"#,
                    )),
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                assert_eq!(response.status, 200);

                let body = response.body.collect().await.expect("Should have body");
                let body_str = String::from_utf8_lossy(&body);
                assert!(
                    body_str.contains(r#""received":true"#),
                    "Body should contain received:true: {}",
                    body_str
                );
                assert!(
                    body_str.contains(r#""name":"test""#),
                    "Body should contain name:test: {}",
                    body_str
                );
                assert!(
                    body_str.contains(r#""count":3"#),
                    "Body should contain count:3: {}",
                    body_str
                );
            })
            .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_request_headers() {
            run_in_local(|| async {
                let script = r#"
                    addEventListener('fetch', (event) => {
                        const auth = event.request.headers.get('Authorization');
                        const custom = event.request.headers.get('X-Custom-Header');
                        event.respondWith(new Response(JSON.stringify({
                            auth: auth,
                            custom: custom
                        })));
                    });
                "#;

                let script_obj = Script::new(script);
                let mut worker = <$worker>::new(script_obj, None)
                    .await
                    .expect("Worker should initialize");

                let mut headers = HashMap::new();
                headers.insert("Authorization".to_string(), "Bearer token123".to_string());
                headers.insert("X-Custom-Header".to_string(), "custom-value".to_string());

                let request = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers,
                    body: RequestBody::None,
                };

                let (task, rx) = Task::fetch(request);
                worker.exec(task).await.expect("Task should execute");

                let response = rx.await.expect("Should receive response");
                let body = response.body.collect().await.expect("Should have body");
                let body_str = String::from_utf8_lossy(&body);
                assert!(
                    body_str.contains("Bearer token123"),
                    "Body should contain auth token: {}",
                    body_str
                );
                assert!(
                    body_str.contains("custom-value"),
                    "Body should contain custom header: {}",
                    body_str
                );
            })
            .await;
        }
    };
}

/// Generate benchmark functions for a runtime
///
/// This macro generates standard benchmarks using Criterion.
/// Usage in benches/worker_benchmark.rs:
///
/// ```ignore
/// use criterion::{criterion_group, criterion_main, Criterion};
/// use openworkers_runner::generate_worker_benchmarks;
/// use your_runtime::Worker;
///
/// generate_worker_benchmarks!(Worker);
///
/// criterion_group!(benches, worker_benchmarks);
/// criterion_main!(benches);
/// ```
#[macro_export]
macro_rules! generate_worker_benchmarks {
    ($worker:ty) => {
        use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task, Worker as _};
        use std::collections::HashMap;

        pub fn worker_benchmarks(c: &mut Criterion) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let mut group = c.benchmark_group("Worker");

            group.bench_function("new", |b| {
                b.iter(|| {
                    rt.block_on(async {
                        let local = tokio::task::LocalSet::new();
                        local
                            .run_until(async {
                                let script = Script::new(
                                    r#"addEventListener('fetch', (e) => e.respondWith(new Response('OK')));"#,
                                );
                                <$worker>::new(script, None).await.unwrap()
                            })
                            .await
                    })
                })
            });

            group.bench_function("exec_simple_response", |b| {
                let mut worker = rt.block_on(async {
                    let local = tokio::task::LocalSet::new();
                    local
                        .run_until(async {
                            let script = Script::new(
                                r#"addEventListener('fetch', (e) => e.respondWith(new Response('OK')));"#,
                            );
                            <$worker>::new(script, None).await.unwrap()
                        })
                        .await
                });

                b.iter(|| {
                    rt.block_on(async {
                        let local = tokio::task::LocalSet::new();
                        local
                            .run_until(async {
                                let req = HttpRequest {
                                    method: HttpMethod::Get,
                                    url: "http://localhost/".to_string(),
                                    headers: HashMap::new(),
                                    body: RequestBody::None,
                                };
                                let (task, rx) = Task::fetch(req);
                                worker.exec(task).await.unwrap();
                                rx.await.unwrap()
                            })
                            .await
                    })
                })
            });

            group.bench_function("exec_json_response", |b| {
                let mut worker = rt.block_on(async {
                    let local = tokio::task::LocalSet::new();
                    local
                        .run_until(async {
                            let script = Script::new(
                                r#"addEventListener('fetch', (e) => e.respondWith(new Response(JSON.stringify({a:1,b:2}))));"#,
                            );
                            <$worker>::new(script, None).await.unwrap()
                        })
                        .await
                });

                b.iter(|| {
                    rt.block_on(async {
                        let local = tokio::task::LocalSet::new();
                        local
                            .run_until(async {
                                let req = HttpRequest {
                                    method: HttpMethod::Get,
                                    url: "http://localhost/".to_string(),
                                    headers: HashMap::new(),
                                    body: RequestBody::None,
                                };
                                let (task, rx) = Task::fetch(req);
                                worker.exec(task).await.unwrap();
                                rx.await.unwrap()
                            })
                            .await
                    })
                })
            });

            group.bench_function("exec_with_headers", |b| {
                let mut worker = rt.block_on(async {
                    let local = tokio::task::LocalSet::new();
                    local
                        .run_until(async {
                            let script = Script::new(
                                r#"addEventListener('fetch', (e) => e.respondWith(new Response('OK', {headers: {'X-A': '1', 'X-B': '2'}})));"#,
                            );
                            <$worker>::new(script, None).await.unwrap()
                        })
                        .await
                });

                b.iter(|| {
                    rt.block_on(async {
                        let local = tokio::task::LocalSet::new();
                        local
                            .run_until(async {
                                let req = HttpRequest {
                                    method: HttpMethod::Get,
                                    url: "http://localhost/".to_string(),
                                    headers: HashMap::new(),
                                    body: RequestBody::None,
                                };
                                let (task, rx) = Task::fetch(req);
                                worker.exec(task).await.unwrap();
                                rx.await.unwrap()
                            })
                            .await
                    })
                })
            });

            group.finish();
        }
    };
}
