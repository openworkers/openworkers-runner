//! Tests for different worker handler patterns
//!
//! This test suite covers all possible ways to define fetch handlers:
//! - addEventListener('fetch', ...) with event.respondWith()
//! - addEventListener('fetch', ...) with return statement
//! - addEventListener('fetch', ...) async handlers
//! - globalThis.default = { fetch() } (transformed from "export default")
//! - globalThis.default = { async fetch() }
//! - globalThis.default = {} (empty - should 501)
//! - globalThis.default = { scheduled } only (should 501 for fetch)
//! - addEventListener + empty globalThis.default (addEventListener wins)
//! - addEventListener + globalThis.default.fetch (default.fetch wins)
//! - No handler at all (should error/501)
//!
//! NOTE: The runtime receives code AFTER transformation by SWC.
//! `export default { ... }` is transformed to `globalThis.default = { ... }`.
//! These tests use the post-transformation format.

#![cfg(not(feature = "wasm"))]

use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use std::time::Duration;

/// Helper to run async tests in a LocalSet (required for tokio spawn_local)
async fn run_local<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let local = tokio::task::LocalSet::new();
    local.run_until(f()).await
}

/// Helper to create a simple GET request
fn make_request() -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: "http://example.com/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    }
}

/// Helper to execute a fetch and get response with timeout
async fn fetch_with_timeout(
    worker: &mut Worker,
    timeout_secs: u64,
) -> Result<(u16, String), String> {
    let (task, rx) = Event::fetch(make_request());
    worker
        .exec(task)
        .await
        .map_err(|e| format!("exec error: {e}"))?;

    let response = tokio::time::timeout(Duration::from_secs(timeout_secs), rx)
        .await
        .map_err(|_| "Timeout waiting for response".to_string())?
        .map_err(|e| format!("channel error: {e}"))?;

    let body = response
        .body
        .collect()
        .await
        .ok_or_else(|| "body collection failed".to_string())?;
    let body_str = String::from_utf8_lossy(&body).to_string();

    Ok((response.status, body_str))
}

// =============================================================================
// addEventListener tests
// =============================================================================

#[tokio::test]
async fn test_add_event_listener_with_respond_with() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response('hello from addEventListener'));
            });"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "hello from addEventListener");
    })
    .await;
}

#[tokio::test]
async fn test_add_event_listener_with_return() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', () => {
                return new Response('hello with return');
            });"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "hello with return");
    })
    .await;
}

#[tokio::test]
async fn test_add_event_listener_async_with_respond_with() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                const response = new Response('async response');
                event.respondWith(response);
            });"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "async response");
    })
    .await;
}

#[tokio::test]
async fn test_add_event_listener_async_with_return() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async () => {
                return new Response('async return');
            });"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "async return");
    })
    .await;
}

// =============================================================================
// globalThis.default = { fetch } tests (ES Modules style, post-transform)
// =============================================================================

#[tokio::test]
async fn test_global_default_fetch() {
    run_local(|| async {
        // This is what `export default { fetch() { ... } }` becomes after SWC transform
        let script = Script::new(
            r#"globalThis.default = {
                fetch(request) {
                    return new Response('hello from globalThis.default');
                }
            };"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "hello from globalThis.default");
    })
    .await;
}

#[tokio::test]
async fn test_global_default_async_fetch() {
    run_local(|| async {
        let script = Script::new(
            r#"globalThis.default = {
                async fetch(request) {
                    return new Response('async globalThis.default');
                }
            };"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "async globalThis.default");
    })
    .await;
}

// =============================================================================
// 501 error cases - no valid fetch handler
// =============================================================================

#[tokio::test]
async fn test_global_default_empty_returns_501() {
    run_local(|| async {
        // `export default {}` becomes `globalThis.default = {}`
        let script = Script::new(r#"globalThis.default = {};"#);

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 501);
        assert!(body.contains("does not implement fetch handler"));
    })
    .await;
}

#[tokio::test]
async fn test_global_default_scheduled_only_returns_501_for_fetch() {
    run_local(|| async {
        let script = Script::new(
            r#"globalThis.default = {
                scheduled(event) {
                    console.log('scheduled');
                }
            };"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 501);
        assert!(body.contains("does not implement fetch handler"));
    })
    .await;
}

#[tokio::test]
async fn test_no_handler_at_all_returns_501() {
    run_local(|| async {
        let script = Script::new(
            r#"// No handler defined at all
            console.log('worker loaded');"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 501);
        assert!(body.contains("does not implement fetch handler"));
    })
    .await;
}

// =============================================================================
// Priority tests - when multiple handlers are defined
// =============================================================================

#[tokio::test]
async fn test_add_event_listener_takes_priority_over_empty_global_default() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response('addEventListener wins'));
            });

            globalThis.default = {};"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "addEventListener wins");
    })
    .await;
}

#[tokio::test]
async fn test_global_default_fetch_takes_priority_over_add_event_listener() {
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response('addEventListener'));
            });

            globalThis.default = {
                fetch(request) {
                    return new Response('globalThis.default wins');
                }
            };"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "globalThis.default wins");
    })
    .await;
}

// =============================================================================
// Edge cases
// =============================================================================

#[tokio::test]
async fn test_global_default_null_returns_501() {
    run_local(|| async {
        let script = Script::new(r#"globalThis.default = null;"#);

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 501);
        assert!(body.contains("does not implement fetch handler"));
    })
    .await;
}

#[tokio::test]
async fn test_global_default_number_returns_501() {
    run_local(|| async {
        let script = Script::new(r#"globalThis.default = 42;"#);

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 501);
        assert!(body.contains("does not implement fetch handler"));
    })
    .await;
}

#[tokio::test]
async fn test_global_default_string_returns_501() {
    run_local(|| async {
        let script = Script::new(r#"globalThis.default = "hello";"#);

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 501);
        assert!(body.contains("does not implement fetch handler"));
    })
    .await;
}

#[tokio::test]
async fn test_global_default_fetch_not_a_function_returns_501() {
    run_local(|| async {
        let script = Script::new(r#"globalThis.default = { fetch: "not a function" };"#);

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 501);
        assert!(body.contains("does not implement fetch handler"));
    })
    .await;
}

// =============================================================================
// Regression tests - respondWith should not be overwritten by async return
// =============================================================================

#[tokio::test]
async fn test_async_handler_respond_with_not_overwritten() {
    // This tests that when an async handler calls respondWith(),
    // the implicit Promise return doesn't overwrite responsePromise
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                event.respondWith(new Response('from respondWith'));
                // async function implicitly returns Promise<undefined>
                // This should NOT overwrite the respondWith response
            });"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "from respondWith");
    })
    .await;
}

#[tokio::test]
async fn test_async_handler_respond_with_after_await() {
    // respondWith called after an await - should still work
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                await Promise.resolve(); // simulate async work
                event.respondWith(new Response('after await'));
            });"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "after await");
    })
    .await;
}

#[tokio::test]
async fn test_async_handler_respond_with_delayed_promise() {
    // respondWith receives a Promise that takes time to resolve
    // The async handler's implicit Promise return should NOT interfere
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', async (event) => {
                // respondWith with a delayed Promise
                event.respondWith(
                    new Promise(resolve => {
                        setTimeout(() => {
                            resolve(new Response('delayed response'));
                        }, 50);
                    })
                );
                // async function returns Promise<undefined>
                // This should NOT overwrite or race with respondWith's Promise
            });"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "delayed response");
    })
    .await;
}

#[tokio::test]
async fn test_respond_with_wins_over_return() {
    // When both respondWith and return are used, respondWith should win
    // (This is the Service Worker spec behavior)
    run_local(|| async {
        let script = Script::new(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response('from respondWith'));
                return new Response('from return');
            });"#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        // respondWith should take priority over return
        assert_eq!(body, "from respondWith");
    })
    .await;
}

// =============================================================================
// Bundler output tests - export { x as default }
// =============================================================================

#[tokio::test]
async fn test_export_as_default_bundler_pattern_transformed() {
    // This is what `export { index_default as default }` becomes after transformation.
    // The transformation is: `export { x as default }` -> `globalThis.default = x`
    run_local(|| async {
        let script = Script::new(
            r#"
            const index_default = {
                fetch(request) {
                    return new Response('from bundler pattern');
                }
            };
            globalThis.default = index_default;
            "#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let (status, body) = fetch_with_timeout(&mut worker, 2)
            .await
            .expect("Should get response");

        assert_eq!(status, 200);
        assert_eq!(body, "from bundler pattern");
    })
    .await;
}
