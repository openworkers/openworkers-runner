//! Worker tests for JavaScript runtimes

#![cfg(not(feature = "wasm"))]

use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

/// Helper to run async tests in a LocalSet (required for tokio spawn_local)
async fn run_local<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let local = tokio::task::LocalSet::new();
    local.run_until(f()).await
}

#[tokio::test(flavor = "current_thread")]
async fn test_simple_response() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response('Hello, World!'));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                const data = { message: 'Hello', value: 42 };
                event.respondWith(new Response(JSON.stringify(data), {
                    headers: { 'Content-Type': 'application/json' }
                }));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
        assert!(body_str.contains("Hello"));
        assert!(body_str.contains("42"));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_custom_status() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response('Not Found', { status: 404 }));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response(event.request.method));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                const url = new URL(event.request.url);
                event.respondWith(new Response(url.pathname));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(
                    Promise.resolve({ async: true })
                        .then(data => new Response(JSON.stringify(data)))
                );
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
        assert!(body_str.contains("true"));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_response_headers() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response('OK', {
                    status: 201,
                    headers: {
                        'X-Custom-Header': 'custom-value',
                        'X-Another': 'another-value'
                    }
                }));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response(null, { status: 204 }));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
        assert!(!response.body.is_stream());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_console_methods() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                console.log('info message');
                console.warn('warn message');
                console.error('error message');
                console.debug('debug message');
                console.trace('trace message');
                event.respondWith(new Response('logged'));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
    run_local(|| async {
        let script = Script::new("this is not valid javascript");
        let result = Worker::new(script, None).await;
        assert!(result.is_err(), "Invalid script should fail");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_response_body_is_streamed() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response('Hello, streaming!'));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
        assert!(response.body.is_stream());

        let body = response.body.collect().await.expect("Should have body");
        assert_eq!(String::from_utf8_lossy(&body), "Hello, streaming!");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_multiple_requests() {
    run_local(|| async {
        let script = Script::new(
            r#"let counter = 0;
            addEventListener('fetch', (event) => {
                counter++;
                event.respondWith(new Response('Request ' + counter));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const body = await event.request.text();
                event.respondWith(new Response('Received: ' + body));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
        let body = response.body.collect().await.expect("Should have body");
        assert_eq!(String::from_utf8_lossy(&body), "Received: Hello, World!");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_request_body_json() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const data = await event.request.json();
                event.respondWith(new Response(JSON.stringify({
                    received: true,
                    name: data.name,
                    count: data.items.length
                }), {
                    headers: { 'Content-Type': 'application/json' }
                }));
            });"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        let request = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers,
            body: RequestBody::Bytes(bytes::Bytes::from(r#"{"name":"test","items":[1,2,3]}"#)),
        };

        let (task, rx) = Task::fetch(request);
        worker.exec(task).await.expect("Task should execute");

        let response = rx.await.expect("Should receive response");
        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains(r#""received":true"#));
        assert!(body_str.contains(r#""name":"test""#));
        assert!(body_str.contains(r#""count":3"#));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_request_headers() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                const auth = event.request.headers.get('Authorization');
                const custom = event.request.headers.get('X-Custom-Header');
                event.respondWith(new Response(JSON.stringify({ auth, custom })));
            });"#,
        );

        let mut worker = Worker::new(script, None)
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
        assert!(body_str.contains("Bearer token123"));
        assert!(body_str.contains("custom-value"));
    })
    .await;
}
