//! A request holds its worker slot until its JS ends, also when the runner
//! stops waiting for it at the external timeout. Otherwise each timed-out
//! request leaves JS running beside a free slot, and the runner takes more
//! work than its slots allow.

use openworkers_core::{
    Event, FetchInit, HttpMethod, HttpRequest, RequestBody, RuntimeLimits, TerminationReason,
};
use openworkers_runner::store::{CodeType, WorkerSource, WorkerWithBindings};
use openworkers_runner::task_executor::{TaskExecutionConfig, execute_task_await};
use openworkers_runner::worker::prepare_worker;
use openworkers_runtime_v8::{PinnedPoolConfig, init_pinned_pool};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Answers after 300 ms, far past the 50 ms external timeout.
const SLOW: &str = r#"
    addEventListener('fetch', (e) => e.respondWith(
        new Promise((resolve) => setTimeout(() => resolve(new Response('late')), 300))
    ));
"#;

#[tokio::test]
async fn a_request_past_the_external_timeout_keeps_its_slot_until_its_js_ends() {
    let limits = RuntimeLimits {
        max_cpu_time_ms: 0,
        max_wall_clock_time_ms: 1_000,
        ..Default::default()
    };

    init_pinned_pool(PinnedPoolConfig {
        max_per_thread: 1,
        max_per_owner: None,
        max_concurrent_per_isolate: 1,
        max_cached_contexts: 10,
        overcommit: true,
        max_context_reuses: 100,
        limits: limits.clone(),
    });

    let worker_data = WorkerWithBindings {
        id: "slot-worker".to_string(),
        name: None,
        user_id: "owner".to_string(),
        code: WorkerSource::Bytes(SLOW.as_bytes().to_vec()),
        code_type: CodeType::Javascript,
        version: 1,
        env: HashMap::new(),
        bindings: vec![],
        env_updated_at: None,
    };
    let prepared = prepare_worker(&worker_data, &limits).unwrap();

    let (res_tx, _res_rx) = tokio::sync::oneshot::channel();
    let request = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let slots = Arc::new(Semaphore::new(1));
    let (global_log_tx, _global_log_rx) = std::sync::mpsc::channel();

    let config = TaskExecutionConfig {
        worker_data,
        permit: Arc::clone(&slots).try_acquire_owned().unwrap(),
        task: Event::Fetch(Some(FetchInit::new(request, res_tx))),
        // Never connects: the worker has no database binding
        db_pool: sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap(),
        global_log_tx,
        limits,
        external_timeout_ms: Some(50),
        abort: Some(tokio_util::sync::CancellationToken::new()),
        span: tracing::Span::none(),
    };

    let result = execute_task_await(config, prepared).await;

    assert_eq!(result, Err(TerminationReason::WallClockTimeout));
    assert_eq!(
        slots.available_permits(),
        0,
        "the JS still runs, so its slot stays taken"
    );

    // The runtime's own wall clock (1 s) ends the JS at the latest
    let _slot = tokio::time::timeout(Duration::from_secs(3), slots.acquire())
        .await
        .expect("the slot comes back once the JS ends")
        .unwrap();
}
