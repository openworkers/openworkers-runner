use std::sync::Arc;
#[cfg(feature = "v8")]
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;
use tracing::Instrument;

use crate::log::WorkerLogHandler;
#[cfg(feature = "v8")]
use crate::ops::LogTx;
use crate::ops::{DbPool, RunnerOperations};
#[cfg(feature = "v8")]
use crate::store::CodeType;
use crate::store::WorkerWithBindings;
#[cfg(feature = "v8")]
use crate::worker::create_worker;
use crate::worker::{PreparedWorker, Worker, create_cached_worker};
use crate::worker_pool::{TaskPermit, WORKER_POOL};

use openworkers_core::{Event, RuntimeLimits, TerminationReason};

/// V8 execution mode - controls how isolates are managed
#[cfg(feature = "v8")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum V8ExecuteMode {
    /// Thread-pinned pool with per-owner isolation (default, best performance)
    #[default]
    Pinned,
    /// Fresh isolate per request (no caching, useful for debugging)
    Oneshot,
}

/// Cached execution mode (read once from environment)
#[cfg(feature = "v8")]
static V8_EXECUTE_MODE: OnceLock<V8ExecuteMode> = OnceLock::new();

#[cfg(feature = "v8")]
impl V8ExecuteMode {
    /// Get the execution mode (cached, read from V8_EXECUTE env var on first call)
    pub fn get() -> Self {
        *V8_EXECUTE_MODE.get_or_init(|| {
            match std::env::var("V8_EXECUTE")
                .ok()
                .map(|s| s.to_uppercase())
                .as_deref()
            {
                Some("PINNED") => Self::Pinned,
                Some("ONESHOT") => Self::Oneshot,
                _ => Self::default(),
            }
        })
    }
}

pub const DEFAULT_CPU_TIME_MS: u64 = 100;

// In test mode, use shorter timeout to fail fast
#[cfg(test)]
pub const DEFAULT_WALL_CLOCK_TIME_MS: u64 = 5_000;

#[cfg(not(test))]
pub const DEFAULT_WALL_CLOCK_TIME_MS: u64 = 60_000;

/// Configuration for executing a task
pub struct TaskExecutionConfig {
    pub worker_data: WorkerWithBindings,
    pub permit: OwnedSemaphorePermit,
    pub task: Event,
    pub db_pool: DbPool,
    pub global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
    pub limits: RuntimeLimits,
    pub external_timeout_ms: Option<u64>,
    /// Cancelled when the client goes away, to stop ops nobody will read.
    pub abort: Option<tokio_util::sync::CancellationToken>,
    pub span: tracing::Span,
}

impl TaskExecutionConfig {
    pub fn default_limits() -> RuntimeLimits {
        RuntimeLimits {
            max_cpu_time_ms: DEFAULT_CPU_TIME_MS,
            max_wall_clock_time_ms: DEFAULT_WALL_CLOCK_TIME_MS,
            ..Default::default()
        }
    }
}

/// Components needed for task execution, separated for ownership management.
struct TaskComponents {
    prepared: PreparedWorker,
    ops: Arc<RunnerOperations>,
    log_handler: WorkerLogHandler,
    /// Raw log sender for warm hit callback (to update cached ops)
    #[cfg(feature = "v8")]
    log_tx: LogTx,
    /// Tracing span for warm hit callback
    #[cfg(feature = "v8")]
    span: tracing::Span,
}

/// Set up logging and the operations handle around a prepared script.
fn task_components(config: &TaskExecutionConfig, prepared: PreparedWorker) -> TaskComponents {
    let (log_tx, log_handler) =
        crate::log::create_log_handler(config.worker_data.id.clone(), config.global_log_tx.clone());

    let ops = Arc::new(
        RunnerOperations::new()
            .with_worker_id(config.worker_data.id.clone())
            .with_log_tx(log_tx.clone())
            .with_bindings(config.worker_data.bindings.clone())
            .with_db_pool(config.db_pool.clone())
            .with_span(config.span.clone()),
    );

    TaskComponents {
        prepared,
        ops,
        log_handler,
        #[cfg(feature = "v8")]
        log_tx,
        #[cfg(feature = "v8")]
        span: config.span.clone(),
    }
}

/// Execute a task with optional external timeout (Worker-based)
async fn run_task_with_timeout_worker(
    worker: &mut Worker,
    task: Event,
    external_timeout_ms: Option<u64>,
) -> Result<(), TerminationReason> {
    match external_timeout_ms {
        Some(timeout_ms) => {
            let timeout_duration = Duration::from_millis(timeout_ms);

            match tokio::time::timeout(timeout_duration, worker.exec(task)).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::error!(
                        "Task execution timeout after {}ms (external timeout)",
                        timeout_ms
                    );
                    Err(TerminationReason::WallClockTimeout)
                }
            }
        }
        None => worker.exec(task).await,
    }
}

/// Workers already warned about, for a warning once per worker and process.
#[cfg(feature = "v8")]
static LATE_RESPOND_WITH_WARNED: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = once_cell::sync::Lazy::new(Default::default);

/// A fetch listener called respondWith after the dispatch: counted on every
/// event, logged once per worker. Answers whether it logged.
#[cfg(feature = "v8")]
fn report_late_respond_with(worker_id: &str, marks: openworkers_runtime_v8::ListenerMarks) -> bool {
    crate::metrics::record_late_respond_with(worker_id, marks.after_settle);

    let first = LATE_RESPOND_WITH_WARNED
        .lock()
        .unwrap()
        .insert(worker_id.to_string());

    if first {
        tracing::warn!(
            worker_id,
            after_settle = marks.after_settle,
            "fetch listener called respondWith after the dispatch ended; the Service Worker spec and Cloudflare refuse this"
        );
    }

    first
}

/// Execute a task using the thread-pinned isolate pool (recommended for V8 workloads)
///
/// This version uses thread-pinned pools from openworkers-runtime-v8, which provides:
/// - Zero contention (each thread has its own pool)
/// - Round-robin distribution across threads (via WORKER_POOL)
/// - Per-owner isolation (isolates tagged with owner_id)
/// - LRU eviction of idle isolates
///
/// Execution mode is controlled by V8_EXECUTE env var:
/// - PINNED (default): Thread-pinned pool, best performance
/// - ONESHOT: Fresh isolate per request (no caching)
///
/// Execution steps:
/// 1. Setup logging around the script the caller prepared
/// 2. Round-robin thread selection (WORKER_POOL)
/// 3. Acquire/create isolate based on V8_EXECUTE mode
/// 4. Execute task with v8::Locker
/// 5. Release/drop isolate
/// 6. Flush logs
#[cfg(feature = "v8")]
pub async fn execute_task_await_v8_pooled(
    config: TaskExecutionConfig,
    prepared: PreparedWorker,
) -> Result<(), TerminationReason> {
    let components = task_components(&config, prepared);

    // Capture JS code for background code cache creation (before script is moved)
    let js_code_for_snapshot = components
        .prepared
        .script
        .code
        .as_js()
        .map(|s| s.to_string());
    let worker_id_for_snapshot = config.worker_data.id.clone();
    let version_for_snapshot = config.worker_data.version;

    // Use user_id (tenant) for isolate pool isolation instead of worker_id
    // This prevents a single tenant from monopolizing resources via multiple workers
    let owner_id = config.worker_data.user_id.clone();
    let task = config.task;
    let limits = config.limits;
    let execute_mode = V8ExecuteMode::get();
    let external_timeout_ms = config.external_timeout_ms;
    let abort = config.abort.clone();
    let abort_for_task = abort.clone();
    let span = config.span.clone();

    // The pooled task holds the slot until its JS ends: after the timeout
    // below the JS still runs, and its slot must stay taken
    let permit = config.permit;

    // Round-robin dispatch across V8 threads for better parallelism under load
    let execution = WORKER_POOL.spawn_await(move || {
        async move {
            let _permit = TaskPermit::new(permit);

            let result = match execute_mode {
                V8ExecuteMode::Pinned => {
                    // Thread-pinned pool (default, best performance)
                    // Pass worker_id + version for warm context caching.
                    // The warm hit callback updates the cached ops' per-request state
                    // so the existing event loop sends logs to the new handler.
                    let warm_log_tx = components.log_tx.clone();
                    let warm_span = components.span.clone();
                    let on_warm_hit: openworkers_runtime_v8::WarmHitCallback =
                        Box::new(move |cached_ops| {
                            // Downcast to RunnerOperations to call update_request.
                            // This updates the cached ops' per-request state (log_tx, span)
                            // so the existing event loop sends logs to the new handler.
                            if let Some(runner_ops) =
                                cached_ops.as_any().downcast_ref::<RunnerOperations>()
                            {
                                runner_ops.update_request(warm_log_tx, warm_span);
                            }
                        });

                    let marked_worker = worker_id_for_snapshot.clone();
                    let on_marks: openworkers_runtime_v8::MarksCallback = Box::new(move |marks| {
                        report_late_respond_with(&marked_worker, marks);
                    });

                    openworkers_runtime_v8::execute_pinned(
                        openworkers_runtime_v8::PinnedExecuteRequest {
                            owner_id,
                            worker_id: worker_id_for_snapshot.clone(),
                            version: version_for_snapshot,
                            script: components.prepared.script,
                            ops: components.ops,
                            task,
                            on_warm_hit: Some(on_warm_hit),
                            env_updated_at: config.worker_data.env_updated_at,
                            abort: abort_for_task,
                            on_marks: Some(on_marks),
                        },
                    )
                    .await
                }
                V8ExecuteMode::Oneshot => {
                    // Serialize OwnedIsolate creation per thread.
                    // V8 requires LIFO ordering for Isolate::Enter/Exit. OwnedIsolate
                    // auto-enters on creation and auto-exits on drop, so concurrent
                    // isolates on the same thread crash if dropped out of order.
                    thread_local! {
                        static ONESHOT_GUARD: Arc<tokio::sync::Semaphore> =
                            Arc::new(tokio::sync::Semaphore::new(1));
                    }

                    let guard = ONESHOT_GUARD.with(Arc::clone);
                    let _permit = guard
                        .acquire()
                        .await
                        .map_err(|_| TerminationReason::Other("Oneshot semaphore closed".into()))?;

                    // Fresh isolate per request (no caching)
                    let mut worker =
                        create_worker(components.prepared.script, limits, components.ops)
                            .await
                            .map_err(|err| {
                                tracing::error!("Failed to create worker: {err:?}");
                                err
                            })?;

                    worker.exec(task).await
                }
            };

            // CRITICAL: Flush logs before returning
            components.log_handler.flush();

            // After successful JS execution, create code cache in background
            if result.is_ok()
                && let Some(js_code) = js_code_for_snapshot
            {
                let worker_id = worker_id_for_snapshot;
                let version = version_for_snapshot;

                tokio::task::spawn_blocking(
                    move || match openworkers_runtime_v8::create_code_cache(&js_code) {
                        Ok(cache) => {
                            let packed = openworkers_runtime_v8::pack_code_cache(&js_code, &cache);
                            crate::code_cache::put(&worker_id, version, &packed);
                            tracing::debug!(
                                "Created code cache: worker={}, version={}, size={}",
                                crate::utils::short_id(&worker_id),
                                version,
                                packed.len()
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to create code cache for worker={}: {}",
                                crate::utils::short_id(&worker_id),
                                e
                            );
                        }
                    },
                );
            }

            result
        }
        .instrument(span)
    });

    let joined = match external_timeout_ms {
        Some(timeout_ms) => {
            match tokio::time::timeout(Duration::from_millis(timeout_ms), execution).await {
                Ok(joined) => joined,
                Err(_) => {
                    tracing::error!(
                        "Task execution timeout after {}ms (external timeout); its slot stays taken until its JS ends",
                        timeout_ms
                    );

                    // The task runs on; cancelling its scope settles the op it waits on.
                    if let Some(abort) = &abort {
                        abort.cancel();
                    }

                    return Err(TerminationReason::WallClockTimeout);
                }
            }
        }
        None => execution.await,
    };

    joined.unwrap_or_else(|_| {
        tracing::error!("Worker pool channel closed unexpectedly");

        Err(TerminationReason::Other(
            "Worker pool channel closed".to_string(),
        ))
    })
}

/// Execute a task in the worker pool and await its completion.
///
/// A JavaScript worker goes to `execute_task_await_v8_pooled` where that
/// backend is in the build; every other worker gets a backend instance of its
/// own, which for v8 would cost the ~3-5ms an isolate takes to create.
///
/// Execution steps:
/// 1. Setup logging around the script the caller prepared
/// 2. Create the worker with runtime limits
/// 3. Execute task
/// 4. Flush logs
/// 5. Auto-release permit and notify drain monitor
pub async fn execute_task_await(
    config: TaskExecutionConfig,
    prepared: PreparedWorker,
) -> Result<(), TerminationReason> {
    // Only v8 has an isolate pool to reuse, and only its own guests reach it
    #[cfg(feature = "v8")]
    if config.worker_data.code_type != CodeType::Wasm {
        return execute_task_await_v8_pooled(config, prepared).await;
    }

    let components = task_components(&config, prepared);

    let limits = config.limits;
    let task = config.task;
    let external_timeout_ms = config.external_timeout_ms;
    let permit = config.permit;
    let span = config.span.clone();
    let worker_id = config.worker_data.id.clone();
    let version = config.worker_data.version;

    WORKER_POOL
        .spawn_await(move || {
            async move {
                // Wrap permit to automatically notify drain monitor on drop
                let _permit = TaskPermit::new(permit);

                let mut worker = create_cached_worker(
                    components.prepared,
                    limits,
                    components.ops,
                    &worker_id,
                    version,
                )
                .await
                .map_err(|err| {
                    tracing::error!("Failed to create worker: {err:?}");
                    err
                })?;

                let result =
                    run_task_with_timeout_worker(&mut worker, task, external_timeout_ms).await;

                // CRITICAL: Flush logs before worker is dropped to prevent log loss
                components.log_handler.flush();

                // TaskPermit is automatically dropped here, releasing the semaphore
                // and notifying the drain monitor

                result
            }
            .instrument(span)
        })
        .await
        .unwrap_or_else(|_| {
            tracing::error!("Worker pool channel closed unexpectedly");
            Err(TerminationReason::Other(
                "Worker pool channel closed".to_string(),
            ))
        })
}

#[cfg(all(test, feature = "v8"))]
mod late_respond_with_tests {
    use super::report_late_respond_with;
    use openworkers_runtime_v8::ListenerMarks;

    #[test]
    fn a_worker_is_logged_once_and_counted_each_time() {
        let marks = ListenerMarks {
            late: true,
            after_settle: false,
        };

        assert!(report_late_respond_with("late-worker-a", marks));
        assert!(!report_late_respond_with("late-worker-a", marks));
        assert!(report_late_respond_with("late-worker-b", marks));
    }
}
