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

use openworkers_core::{
    BindingInfo, DatabaseOp, DatabaseResult, Event, HttpMethod, HttpRequest, HttpResponse,
    OpFuture, OperationsHandler, RequestBody, ResponseBody, RuntimeLimits, Script,
};
use openworkers_runner::ops::RunnerOperations;
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, execute_pinned, init_pinned_pool,
};
use std::collections::HashMap;
use std::sync::{Arc, Once};
use std::time::Duration;

static INIT: Once = Once::new();

fn init_pool() {
    INIT.call_once(|| {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });
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

// =============================================================================
// Concurrent interleaving STRESS test — reproduces production segfault
// =============================================================================

/// Mock ops handler that responds to fetch() with a configurable delay.
///
/// The delay simulates real I/O (DB query, external HTTP) and ensures that
/// `tokio::spawn` + `tokio::time::sleep` in the scheduler creates a genuine
/// async yield. The callback arrives on `callback_tx` after the delay, forcing
/// the event loop's `poll_fn` to return Pending and interleave with other tasks.
struct DelayedFetchOps {
    delay_ms: u64,
}

impl OperationsHandler for DelayedFetchOps {
    fn handle_fetch(&self, _request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        let delay = self.delay_ms;
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/plain".to_string())],
                body: ResponseBody::Bytes(bytes::Bytes::from_static(b"mock-ok")),
            })
        })
    }
}

/// Mock ops handler that responds to env.DB.query() with a configurable delay.
///
/// Returns DatabaseResult::Rows with a JSON array. The delay simulates a real
/// PostgreSQL round-trip. This goes through the BindingDatabase path:
///   JS env.DB.query() → __nativeBindingDatabase → SchedulerMessage::BindingDatabase
///   → tokio::spawn → handle_binding_database → CallbackMessage::DatabaseResult
///   → populate_database_result (v8::json::parse) → dispatch_binding_callbacks
///
/// This is the EXACT path that segfaults in production.
struct DelayedDbOps {
    delay_ms: u64,
}

impl OperationsHandler for DelayedDbOps {
    fn handle_binding_database(
        &self,
        _binding: &str,
        _op: DatabaseOp,
    ) -> OpFuture<'_, DatabaseResult> {
        let delay = self.delay_ms;
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            DatabaseResult::Rows(r#"[{"id":1,"value":"mock"}]"#.to_string())
        })
    }
}

/// Helper: execute a script with custom ops via execute_pinned, return (status, body).
async fn pinned_fetch_with_ops(
    worker_id: &str,
    version: i32,
    script: Script,
    ops: Arc<dyn OperationsHandler>,
) -> (u16, String) {
    let (task, rx) = Event::fetch(make_request());

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

    let response = tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("Should receive response in time")
        .expect("Channel should not be closed");

    let body = response.body.collect().await.expect("Should collect body");
    let body_str = String::from_utf8_lossy(&body).to_string();

    (response.status, body_str)
}

/// Stress test: concurrent requests using fetch() with mocked I/O delay.
///
/// Production segfaults inside `v8__Isolate__Enter()` after ~28 requests with
/// parallelism >= 2. The root cause is suspected to be V8 entry_stack corruption
/// from non-LIFO Enter/Exit ordering with cooperative scheduling.
///
/// Previous attempts with setTimeout failed to reproduce because timer callbacks
/// are handled synchronously within the runtime — the event loop completes in
/// one poll cycle per timer, so tasks never truly interleave their IsolateScope
/// enter/exit blocks.
///
/// This test uses fetch() which goes through the EXTERNAL callback path:
///   1. JS calls fetch("http://mock") → scheduler dispatches via tokio::spawn
///   2. DelayedFetchOps::handle_fetch() sleeps 20ms (simulates I/O)
///   3. Response arrives as CallbackMessage::FetchStreamingSuccess on callback_tx
///   4. Event loop's poll_fn receives it → process_single_callback (IsolateScope)
///      → pump_and_checkpoint (IsolateScope) → check_exit (IsolateScope)
///
/// During step 2, the event loop returns Pending. The LocalSet switches to the
/// other concurrent task, which also starts a fetch and goes Pending. Now both
/// tasks have active Lockers on the same thread, and their IsolateScope
/// enter/exits truly interleave as callbacks arrive at staggered times.
#[tokio::test]
async fn test_concurrent_interleaving_stress() {
    init_pool();

    run_local(|| async {
        // Worker that calls fetch() — goes through the external callback path
        // (tokio::spawn → ops.handle_fetch → callback_tx → process_single_callback)
        let script = Script::new(
            r#"
            addEventListener('fetch', (event) => {
                fetch('http://mock/api').then(response => {
                    return response.text();
                }).then(text => {
                    event.respondWith(new Response(text));
                }).catch(err => {
                    event.respondWith(new Response('error: ' + err, { status: 500 }));
                });
            });
            "#,
        );

        // 20ms delay simulates real I/O — long enough for real interleaving
        let ops: Arc<dyn OperationsHandler> = Arc::new(DelayedFetchOps { delay_ms: 20 });

        let num_workers = 4;
        let num_rounds = 50;

        for round in 0..num_rounds {
            let mut handles = Vec::new();

            for worker_idx in 0..num_workers {
                let script = script.clone();
                let worker_id = format!("stress-{}", worker_idx);
                let ops = ops.clone();

                handles.push(tokio::task::spawn_local(async move {
                    pinned_fetch_with_ops(&worker_id, 1, script, ops).await
                }));
            }

            for (idx, handle) in handles.into_iter().enumerate() {
                let (status, body) = handle
                    .await
                    .unwrap_or_else(|e| panic!("round {round} worker {idx} panicked: {e}"));
                assert_eq!(
                    status, 200,
                    "round {round} worker {idx}: expected 200, got {status} body={body}"
                );
                assert_eq!(
                    body, "mock-ok",
                    "round {round} worker {idx}: expected 'mock-ok', got '{body}'"
                );
            }
        }
    })
    .await;
}

// =============================================================================
// Database binding stress test — reproduces production segfault path
// =============================================================================

/// Stress test: concurrent requests using env.DB.query() with mocked delay.
///
/// This targets the EXACT callback path that segfaults in production:
///   JS: env.DB.query("SELECT ...") → __nativeBindingDatabase
///     → SchedulerMessage::BindingDatabase → tokio::spawn
///     → handle_binding_database (20ms delay) → CallbackMessage::DatabaseResult
///     → populate_database_result (v8::json::parse on rows JSON)
///     → dispatch_binding_callbacks → Promise resolves → .then(r => r.rows)
///
/// Production crash signature:
///   - 2+ concurrent DB requests on same thread (spawn_local on shared LocalSet)
///   - Segfault in v8__Isolate__Enter() inside Locker::new after ~10-28 requests
///   - Only happens with database workers, not simple API workers
///
/// The DB path exercises more V8 internals per callback than fetch:
///   - v8::json::parse() inside process_single_callback
///   - Object property setting (success, rows, error)
///   - dispatch_binding_callbacks with V8 function calls
#[tokio::test]
async fn test_concurrent_db_interleaving_stress() {
    init_pool();

    run_local(|| async {
        // Worker that calls env.DB.query() — goes through the database binding path
        let script = Script::with_bindings(
            r#"
            addEventListener('fetch', (event) => {
                env.DB.query("SELECT 1 as id, 'hello' as value").then(rows => {
                    event.respondWith(new Response(JSON.stringify(rows)));
                }).catch(err => {
                    event.respondWith(new Response('error: ' + err, { status: 500 }));
                });
            });
            "#,
            None,
            vec![BindingInfo::database("DB")],
        );

        // 20ms delay simulates real PostgreSQL round-trip
        let ops: Arc<dyn OperationsHandler> = Arc::new(DelayedDbOps { delay_ms: 20 });

        let num_workers = 4;
        let num_rounds = 50;

        for round in 0..num_rounds {
            let mut handles = Vec::new();

            for worker_idx in 0..num_workers {
                let script = script.clone();
                let worker_id = format!("db-stress-{}", worker_idx);
                let ops = ops.clone();

                handles.push(tokio::task::spawn_local(async move {
                    pinned_fetch_with_ops(&worker_id, 1, script, ops).await
                }));
            }

            for (idx, handle) in handles.into_iter().enumerate() {
                let (status, body) = handle
                    .await
                    .unwrap_or_else(|e| panic!("round {round} worker {idx} panicked: {e}"));

                assert_eq!(
                    status, 200,
                    "round {round} worker {idx}: expected 200, got {status} body={body}"
                );

                // Verify the DB result was properly parsed
                assert!(
                    body.contains("mock"),
                    "round {round} worker {idx}: expected DB rows, got '{body}'"
                );
            }
        }
    })
    .await;
}
