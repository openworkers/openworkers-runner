//! Web API conformance tests
//!
//! Tests for URL, URLSearchParams, and other Web API polyfills.
//! These should pass on all runtimes.

#![cfg(not(feature = "wasm"))]

use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

async fn run_local<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let local = tokio::task::LocalSet::new();
    local.run_until(f()).await
}

/// Helper: run a JS snippet that returns a string via Response body
async fn eval_js(code: &str) -> String {
    let script = Script::new(code);

    let mut worker = Worker::new(script, None)
        .await
        .expect("Worker should initialize");

    let request = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Event::fetch(request);
    worker.exec(task).await.expect("Task should execute");

    let response = rx.await.expect("Should receive response");
    assert_eq!(response.status, 200);

    let body = response.body.collect().await.expect("Should have body");
    String::from_utf8(body.to_vec()).expect("Body should be valid UTF-8")
}

// ============================================================================
// URL.searchParams mutations sync back to URL
// ============================================================================

#[tokio::test]
async fn test_url_search_params_set_updates_href() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('https://example.com/path');
                url.searchParams.set('key', 'value');
                event.respondWith(new Response(url.href));
            });"#,
        )
        .await;

        assert_eq!(result, "https://example.com/path?key=value");
    })
    .await;
}

#[tokio::test]
async fn test_url_search_params_set_updates_to_string() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('https://example.com/path');
                url.searchParams.set('foo', 'bar');
                event.respondWith(new Response(url.toString()));
            });"#,
        )
        .await;

        assert_eq!(result, "https://example.com/path?foo=bar");
    })
    .await;
}

#[tokio::test]
async fn test_url_search_params_append() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('https://example.com/');
                url.searchParams.append('a', '1');
                url.searchParams.append('b', '2');
                event.respondWith(new Response(url.href));
            });"#,
        )
        .await;

        assert_eq!(result, "https://example.com/?a=1&b=2");
    })
    .await;
}

#[tokio::test]
async fn test_url_search_params_delete() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('https://example.com/?a=1&b=2&c=3');
                url.searchParams.delete('b');
                event.respondWith(new Response(url.href));
            });"#,
        )
        .await;

        assert_eq!(result, "https://example.com/?a=1&c=3");
    })
    .await;
}

#[tokio::test]
async fn test_url_search_params_sort() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('https://example.com/?c=3&a=1&b=2');
                url.searchParams.sort();
                event.respondWith(new Response(url.href));
            });"#,
        )
        .await;

        assert_eq!(result, "https://example.com/?a=1&b=2&c=3");
    })
    .await;
}

#[tokio::test]
async fn test_url_search_params_set_replaces_existing() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('https://example.com/?key=old');
                url.searchParams.set('key', 'new');
                event.respondWith(new Response(url.href));
            });"#,
        )
        .await;

        assert_eq!(result, "https://example.com/?key=new");
    })
    .await;
}

#[tokio::test]
async fn test_url_search_params_multiple_set() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('https://github.com/login/oauth/authorize');
                url.searchParams.set('client_id', 'my-client-id');
                url.searchParams.set('scope', 'read:user');
                event.respondWith(new Response(url.toString()));
            });"#,
        )
        .await;

        assert_eq!(
            result,
            "https://github.com/login/oauth/authorize?client_id=my-client-id&scope=read%3Auser"
        );
    })
    .await;
}

#[tokio::test]
async fn test_url_search_params_preserves_hash() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('https://example.com/path#section');
                url.searchParams.set('key', 'value');
                event.respondWith(new Response(url.href));
            });"#,
        )
        .await;

        assert_eq!(result, "https://example.com/path?key=value#section");
    })
    .await;
}

#[tokio::test]
async fn test_url_search_params_updates_search_property() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('https://example.com/');
                url.searchParams.set('x', '42');
                event.respondWith(new Response(url.search));
            });"#,
        )
        .await;

        assert_eq!(result, "?x=42");
    })
    .await;
}

#[tokio::test]
async fn test_url_search_params_delete_all_clears_search() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('https://example.com/?a=1');
                url.searchParams.delete('a');
                event.respondWith(new Response(url.href));
            });"#,
        )
        .await;

        assert_eq!(result, "https://example.com/");
    })
    .await;
}

// ============================================================================
// URLSearchParams standalone
// ============================================================================

#[tokio::test]
async fn test_url_search_params_standalone_to_string() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const params = new URLSearchParams();
                params.set('a', '1');
                params.set('b', '2');
                event.respondWith(new Response(params.toString()));
            });"#,
        )
        .await;

        assert_eq!(result, "a=1&b=2");
    })
    .await;
}

#[tokio::test]
async fn test_url_search_params_from_string() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const params = new URLSearchParams('?foo=bar&baz=qux');
                event.respondWith(new Response(params.get('foo') + ',' + params.get('baz')));
            });"#,
        )
        .await;

        assert_eq!(result, "bar,qux");
    })
    .await;
}

#[tokio::test]
async fn test_url_search_params_encoding() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const params = new URLSearchParams();
                params.set('q', 'hello world');
                params.set('special', 'a&b=c');
                event.respondWith(new Response(params.toString()));
            });"#,
        )
        .await;

        assert_eq!(result, "q=hello%20world&special=a%26b%3Dc");
    })
    .await;
}

// ============================================================================
// btoa / atob (binary strings, NOT UTF-8)
// ============================================================================

#[tokio::test]
async fn test_btoa_ascii() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                event.respondWith(new Response(btoa('Hello, World!')));
            });"#,
        )
        .await;

        assert_eq!(result, "SGVsbG8sIFdvcmxkIQ==");
    })
    .await;
}

#[tokio::test]
async fn test_btoa_binary_roundtrip() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                // Build a binary string with bytes 0-255
                let binary = '';
                for (let i = 0; i < 256; i++) binary += String.fromCharCode(i);
                const encoded = btoa(binary);
                const decoded = atob(encoded);

                // Verify roundtrip: every byte should match
                let ok = decoded.length === 256;
                for (let i = 0; i < 256 && ok; i++) {
                    if (decoded.charCodeAt(i) !== i) ok = false;
                }

                event.respondWith(new Response(ok ? 'pass' : 'fail'));
            });"#,
        )
        .await;

        assert_eq!(result, "pass");
    })
    .await;
}

#[tokio::test]
async fn test_btoa_hmac_signature_roundtrip() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', async (event) => {
                // Simulate JWT-style sign → btoa → atob → verify
                const key = await crypto.subtle.importKey(
                    'raw',
                    new TextEncoder().encode('secret'),
                    { name: 'HMAC', hash: 'SHA-256' },
                    false,
                    ['sign', 'verify']
                );

                const data = new TextEncoder().encode('header.payload');
                const sig = await crypto.subtle.sign('HMAC', key, data);

                // Roundtrip through btoa/atob (like JWT libraries do)
                const binaryStr = String.fromCharCode(...new Uint8Array(sig));
                const b64 = btoa(binaryStr);
                const decoded = atob(b64);
                const sigBytes = new Uint8Array(decoded.length);
                for (let i = 0; i < decoded.length; i++) {
                    sigBytes[i] = decoded.charCodeAt(i);
                }

                const valid = await crypto.subtle.verify('HMAC', key, sigBytes, data);
                event.respondWith(new Response(valid ? 'pass' : 'fail'));
            });"#,
        )
        .await;

        assert_eq!(result, "pass");
    })
    .await;
}

// ============================================================================
// URL parsing
// ============================================================================

#[tokio::test]
async fn test_url_parse_with_query_and_hash() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('https://example.com:8080/path?key=val#frag');
                const parts = [
                    url.protocol,
                    url.hostname,
                    url.port,
                    url.pathname,
                    url.search,
                    url.hash,
                    url.origin
                ];
                event.respondWith(new Response(JSON.stringify(parts)));
            });"#,
        )
        .await;

        let parts: Vec<String> = serde_json::from_str(&result).unwrap();
        assert_eq!(parts[0], "https:");
        assert_eq!(parts[1], "example.com");
        assert_eq!(parts[2], "8080");
        assert_eq!(parts[3], "/path");
        assert_eq!(parts[4], "?key=val");
        assert_eq!(parts[5], "#frag");
        assert_eq!(parts[6], "https://example.com:8080");
    })
    .await;
}

#[tokio::test]
async fn test_url_relative_resolution() {
    run_local(|| async {
        let result = eval_js(
            r#"addEventListener('fetch', (event) => {
                const url = new URL('/api/v2', 'https://example.com/old/path');
                event.respondWith(new Response(url.href));
            });"#,
        )
        .await;

        assert_eq!(result, "https://example.com/api/v2");
    })
    .await;
}
