//! Tests for warm isolate reuse via the thread-pinned pool.
//!
//! Each test executes the SAME worker_id+version twice via execute_pinned.
//! The second call is a warm hit: reset() is called instead of creating a new context.
//! This validates that per-request state is properly cleaned while module-level
//! state persists across requests.
//!
//! IMPORTANT: On warm hit, only the fetch handler runs — module-level code does NOT
//! re-execute. Counters and state tracking must be inside the handler.

#![cfg(feature = "v8")]

use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, RuntimeLimits, Script};
use openworkers_runner::ops::RunnerOperations;
use openworkers_runtime_v8::{PinnedExecuteRequest, execute_pinned, init_pinned_pool};
use std::collections::HashMap;
use std::sync::{Arc, Once};
use std::time::Duration;

static INIT: Once = Once::new();

fn init_pool() {
    INIT.call_once(|| {
        init_pinned_pool(10, RuntimeLimits::default());
    });
}

/// Helper to run async tests in a LocalSet (required for execute_pinned)
async fn run_local<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let local = tokio::task::LocalSet::new();
    local.run_until(f()).await
}

fn make_request() -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    }
}

/// Helper: execute a script via execute_pinned and return (status, body).
async fn pinned_fetch(worker_id: &str, version: i32, script: Script) -> (u16, String) {
    let (task, rx) = Event::fetch(make_request());
    let ops = Arc::new(RunnerOperations::new());

    let result = execute_pinned(PinnedExecuteRequest {
        owner_id: "test-warm".to_string(),
        worker_id: worker_id.to_string(),
        version,
        script,
        ops,
        task,
        on_warm_hit: None,
    })
    .await;

    result.expect("execute_pinned should succeed");

    let response = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("Should receive response in time")
        .expect("Channel should not be closed");

    let body = response.body.collect().await.expect("Should collect body");
    let body_str = String::from_utf8_lossy(&body).to_string();

    (response.status, body_str)
}

// =============================================================================
// Timer isolation tests
// =============================================================================

/// setTimeout registered in request 1 must NOT fire during request 2.
///
/// Request 1 sets a 5-second timer that would set globalThis.__leaked = true.
/// Request 2 checks that __leaked is undefined (timer callbacks were cleared by reset()).
///
/// NOTE: On warm hit, module-level code does NOT re-execute. The fetch handler
/// uses globalThis.__reqCount (incremented inside the handler) to distinguish requests.
#[tokio::test]
async fn test_warm_reuse_timer_isolation() {
    init_pool();

    run_local(|| async {
        let worker_id = "timer-isolation";
        let script = Script::new(
            r#"
            // Module-level: runs once on cold path only
            globalThis.__reqCount = 0;

            addEventListener('fetch', (event) => {
                globalThis.__reqCount++;
                const reqNum = globalThis.__reqCount;

                if (reqNum === 1) {
                    // Request 1: schedule a timer that would leak if not cleared
                    setTimeout(() => { globalThis.__leaked = true; }, 5000);
                    event.respondWith(new Response('req1'));
                } else {
                    // Request 2: check that the timer callback was cleared
                    const leaked = typeof globalThis.__leaked !== 'undefined';
                    event.respondWith(new Response(leaked ? 'LEAKED' : 'clean'));
                }
            });
            "#,
        );

        let (status, body) = pinned_fetch(worker_id, 1, script.clone()).await;
        assert_eq!(status, 200);
        assert_eq!(body, "req1");

        let (status, body) = pinned_fetch(worker_id, 1, script).await;
        assert_eq!(status, 200);
        assert_eq!(
            body, "clean",
            "Timer callback should have been cleared by reset()"
        );
    })
    .await;
}

/// setInterval registered in request 1 must NOT continue firing in request 2.
#[tokio::test]
async fn test_warm_reuse_interval_isolation() {
    init_pool();

    run_local(|| async {
        let worker_id = "interval-isolation";
        let script = Script::new(
            r#"
            globalThis.__reqCount = 0;
            globalThis.__intervalFired = 0;

            addEventListener('fetch', (event) => {
                globalThis.__reqCount++;
                const reqNum = globalThis.__reqCount;

                if (reqNum === 1) {
                    // Request 1: start an interval that would leak
                    setInterval(() => { globalThis.__intervalFired++; }, 50);
                    event.respondWith(new Response('req1'));
                } else {
                    // Request 2: the interval callback should be gone
                    // Reset counter and wait 150ms to give any surviving interval time to fire
                    globalThis.__intervalFired = 0;
                    setTimeout(() => {
                        const count = globalThis.__intervalFired;
                        event.respondWith(new Response(String(count)));
                    }, 150);
                }
            });
            "#,
        );

        let (status, body) = pinned_fetch(worker_id, 1, script.clone()).await;
        assert_eq!(status, 200);
        assert_eq!(body, "req1");

        let (status, body) = pinned_fetch(worker_id, 1, script).await;
        assert_eq!(status, 200);
        assert_eq!(body, "0", "Interval callback should not fire after reset()");
    })
    .await;
}

// =============================================================================
// State persistence test
// =============================================================================

/// Module-level state should persist across warm reuses (same V8 context).
///
/// A counter incremented in the handler should be 1 on first call and 2 on second.
#[tokio::test]
async fn test_warm_reuse_state_persists() {
    init_pool();

    run_local(|| async {
        let worker_id = "state-persists";
        let script = Script::new(
            r#"
            globalThis.__counter = 0;

            addEventListener('fetch', (event) => {
                globalThis.__counter++;
                event.respondWith(new Response(String(globalThis.__counter)));
            });
            "#,
        );

        let (status, body) = pinned_fetch(worker_id, 1, script.clone()).await;
        assert_eq!(status, 200);
        assert_eq!(body, "1");

        let (status, body) = pinned_fetch(worker_id, 1, script).await;
        assert_eq!(status, 200);
        assert_eq!(
            body, "2",
            "Module-level counter should persist on warm reuse"
        );
    })
    .await;
}

// =============================================================================
// Response state isolation test
// =============================================================================

/// __lastResponse and __requestComplete must be clean on request 2.
///
/// Request 1 returns a custom response. Request 2 verifies that the per-request
/// globals were properly reset (they should be undefined/false before the handler runs).
#[tokio::test]
async fn test_warm_reuse_response_state_clean() {
    init_pool();

    run_local(|| async {
        let worker_id = "response-state-clean";
        let script = Script::new(
            r#"
            globalThis.__reqCount = 0;

            addEventListener('fetch', (event) => {
                globalThis.__reqCount++;
                const reqNum = globalThis.__reqCount;

                if (reqNum === 1) {
                    event.respondWith(new Response('first'));
                } else {
                    // Check that per-request state was cleaned by reset()
                    const lastResp = globalThis.__lastResponse;
                    const reqComplete = globalThis.__requestComplete;
                    const clean = lastResp === undefined && reqComplete === false;
                    event.respondWith(new Response(clean ? 'clean' : 'dirty'));
                }
            });
            "#,
        );

        let (status, body) = pinned_fetch(worker_id, 1, script.clone()).await;
        assert_eq!(status, 200);
        assert_eq!(body, "first");

        let (status, body) = pinned_fetch(worker_id, 1, script).await;
        assert_eq!(status, 200);
        assert_eq!(
            body, "clean",
            "Per-request state should be reset between requests"
        );
    })
    .await;
}

// =============================================================================
// waitUntil + warm reuse tests
// =============================================================================

/// Warm reuse should work after a request with a short waitUntil.
///
/// Request 1 uses waitUntil with a 50ms timer. After drain_waituntil completes,
/// the context should be cached. Request 2 should warm-hit successfully.
#[tokio::test]
async fn test_warm_reuse_after_waituntil() {
    init_pool();

    run_local(|| async {
        let worker_id = "after-waituntil";
        let script = Script::new(
            r#"
            globalThis.__counter = 0;

            addEventListener('fetch', (event) => {
                globalThis.__counter++;
                event.respondWith(new Response(String(globalThis.__counter)));
                event.waitUntil(new Promise(resolve => setTimeout(resolve, 50)));
            });
            "#,
        );

        let (status, body) = pinned_fetch(worker_id, 1, script.clone()).await;
        assert_eq!(status, 200);
        assert_eq!(body, "1");

        let (status, body) = pinned_fetch(worker_id, 1, script).await;
        assert_eq!(status, 200);
        assert_eq!(body, "2", "Should warm-reuse after waitUntil completes");
    })
    .await;
}

/// Warm reuse should work after a request whose waitUntil promise was rejected.
///
/// The rejection is caught by __triggerFetch's catch block, but since respondWith
/// already set __lastResponse, the catch does NOT overwrite the 200 response.
/// After drain_waituntil completes, the context should still be cached.
#[tokio::test]
async fn test_warm_reuse_after_rejected_waituntil() {
    init_pool();

    run_local(|| async {
        let worker_id = "after-rejected-waituntil";
        let script = Script::new(
            r#"
            globalThis.__counter = 0;

            addEventListener('fetch', (event) => {
                globalThis.__counter++;
                event.respondWith(new Response(String(globalThis.__counter)));
                event.waitUntil(Promise.reject(new Error('background failure')));
            });
            "#,
        );

        let (status, body) = pinned_fetch(worker_id, 1, script.clone()).await;
        assert_eq!(status, 200);
        assert_eq!(body, "1");

        let (status, body) = pinned_fetch(worker_id, 1, script).await;
        assert_eq!(status, 200);
        assert_eq!(
            body, "2",
            "Should warm-reuse after waitUntil rejection (rejection is caught)"
        );
    })
    .await;
}

// =============================================================================
// Concurrent interleaving test (V8 Locker + spawn_local)
// =============================================================================

/// Two concurrent execute_pinned calls with async yields on the same thread.
///
/// This reproduces the V8 isolate interleaving bug:
/// 1. Task A creates Locker(X), enters isolate X → GetCurrent() = X
/// 2. Task A's worker does setTimeout(50ms) → event loop returns Pending
/// 3. LocalSet polls Task B → Locker(Y) enters isolate Y → GetCurrent() = Y
/// 4. Task B's worker also yields (setTimeout)
/// 5. Task A resumes, creates HandleScope from X's raw pointer
/// 6. ContextScope::new checks: X's pointer != GetCurrent() (Y) → PANIC
///
/// The panic: "PinnedRef and Context do not belong to the same Isolate"
///
/// This happens because v8::Locker calls Isolate::Enter() which sets the
/// thread-local "current isolate". With cooperative scheduling (spawn_local),
/// multiple Lockers coexist on the same thread with non-LIFO ordering.
#[tokio::test]
async fn test_concurrent_pinned_interleaving() {
    init_pool();

    run_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', (event) => {
                // Force async yield: respond after a delay.
                // During this delay, the event loop returns Pending,
                // allowing the LocalSet to poll another task.
                setTimeout(() => {
                    event.respondWith(new Response('ok'));
                }, 50);
            });
            "#,
        );

        // Spawn two concurrent tasks on the same LocalSet (same OS thread).
        // They get DIFFERENT isolates because they have different worker_ids.
        let handle_a = tokio::task::spawn_local({
            let script = script.clone();
            async move { pinned_fetch("interleave-a", 1, script).await }
        });

        let handle_b = tokio::task::spawn_local({
            let script = script.clone();
            async move { pinned_fetch("interleave-b", 1, script).await }
        });

        let (result_a, result_b) = tokio::join!(handle_a, handle_b);

        let (status_a, body_a) = result_a.expect("Task A should not panic");
        let (status_b, body_b) = result_b.expect("Task B should not panic");

        assert_eq!(status_a, 200);
        assert_eq!(body_a, "ok");
        assert_eq!(status_b, 200);
        assert_eq!(body_b, "ok");
    })
    .await;
}
