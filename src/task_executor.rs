use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;

use crate::log::WorkerLogHandler;
use crate::ops::{DbPool, RunnerOperations};
use crate::store::{CodeType, WorkerWithBindings};
use crate::worker::{Worker, create_worker, prepare_script};
use crate::worker_pool::{TaskPermit, WORKER_POOL};

use openworkers_core::{Event, RuntimeLimits, Script, TerminationReason};

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
    script: Script,
    ops: Arc<RunnerOperations>,
    code_type: CodeType,
    log_handler: WorkerLogHandler,
}

/// Prepare task components: parse script, setup logging, and create operations handle.
///
/// Returns None if script preparation fails (error is logged).
fn prepare_task_components(config: &TaskExecutionConfig) -> Option<TaskComponents> {
    let script = match prepare_script(&config.worker_data) {
        Ok(s) => s,
        Err(err) => {
            log::error!("Failed to prepare script: {err:?}");
            return None;
        }
    };

    let (log_tx, log_handler) =
        crate::log::create_log_handler(config.worker_data.id.clone(), config.global_log_tx.clone());

    let ops = Arc::new(
        RunnerOperations::new()
            .with_worker_id(config.worker_data.id.clone())
            .with_log_tx(log_tx)
            .with_bindings(config.worker_data.bindings.clone())
            .with_db_pool(config.db_pool.clone()),
    );

    Some(TaskComponents {
        script,
        ops,
        code_type: config.worker_data.code_type.clone(),
        log_handler,
    })
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
                    log::error!(
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

/// Execute a task using the thread-pinned isolate pool (recommended for V8 workloads)
///
/// This version uses thread-pinned pools from openworkers-runtime-v8, which provides:
/// - Zero contention (each thread has its own pool)
/// - Round-robin distribution across threads (via WORKER_POOL)
/// - Per-owner isolation (isolates tagged with owner_id)
/// - LRU eviction of idle isolates
/// - Queue with backpressure when at capacity
///
/// Execution steps:
/// 1. Parse script (fail fast)
/// 2. Setup logging
/// 3. Round-robin thread selection (WORKER_POOL)
/// 4. Acquire isolate for this owner from thread-local pool
/// 5. Execute task with v8::Locker
/// 6. Release isolate to thread-local pool
/// 7. Flush logs
#[cfg(feature = "v8")]
pub async fn execute_task_await_v8_pooled(
    config: TaskExecutionConfig,
) -> Result<(), TerminationReason> {
    let components = prepare_task_components(&config)
        .ok_or_else(|| TerminationReason::Other("Failed to prepare script".to_string()))?;

    // Use user_id (tenant) for isolate pool isolation instead of worker_id
    // This prevents a single tenant from monopolizing resources via multiple workers
    let owner_id = config.worker_data.user_id.clone();
    let task = config.task;
    let permit = config.permit;

    WORKER_POOL
        .spawn_await(move || async move {
            // Wrap permit to automatically notify drain monitor on drop
            let _permit = TaskPermit::new(permit);

            // Use the thread-pinned pool execution API from runtime-v8
            // owner_id is the tenant (user_id), not the worker_id
            let result = openworkers_runtime_v8::execute_pinned(
                &owner_id,
                components.script,
                components.ops,
                task,
            )
            .await;

            // CRITICAL: Flush logs before returning
            components.log_handler.flush();

            result
        })
        .await
        .unwrap_or_else(|_| {
            log::error!("Worker pool channel closed unexpectedly");
            Err(TerminationReason::Other(
                "Worker pool channel closed".to_string(),
            ))
        })
}

/// Execute a task in the worker pool and await its completion (legacy Worker-based).
///
/// NOTE: This creates a new V8 isolate per request (~3-5ms overhead).
/// For V8 workloads, prefer `execute_task_await_v8_pooled` which reuses isolates.
///
/// Execution steps:
/// 1. Parse script (fail fast)
/// 2. Setup logging
/// 3. Create V8 isolate with runtime limits
/// 4. Execute task
/// 5. Flush logs
/// 6. Auto-release permit and notify drain monitor
pub async fn execute_task_await(config: TaskExecutionConfig) -> Result<(), TerminationReason> {
    // For V8-only builds, prefer the pooled version
    #[cfg(all(feature = "v8", not(feature = "wasm")))]
    {
        #[allow(clippy::needless_return)]
        return execute_task_await_v8_pooled(config).await;
    }

    // Fallback for WASM or dual-runtime builds
    #[cfg(not(all(feature = "v8", not(feature = "wasm"))))]
    {
        let components = prepare_task_components(&config)
            .ok_or_else(|| TerminationReason::Other("Failed to prepare script".to_string()))?;

        let limits = config.limits;
        let task = config.task;
        let external_timeout_ms = config.external_timeout_ms;
        let permit = config.permit;

        WORKER_POOL
            .spawn_await(move || async move {
                // Wrap permit to automatically notify drain monitor on drop
                let _permit = TaskPermit::new(permit);

                let mut worker = crate::worker::create_worker(
                    components.script,
                    limits,
                    components.ops,
                    &components.code_type,
                )
                .await
                .map_err(|err| {
                    log::error!("Failed to create worker: {err:?}");
                    err
                })?;

                let result =
                    run_task_with_timeout_worker(&mut worker, task, external_timeout_ms).await;

                // CRITICAL: Flush logs before worker is dropped to prevent log loss
                components.log_handler.flush();

                // TaskPermit is automatically dropped here, releasing the semaphore
                // and notifying the drain monitor

                result
            })
            .await
            .unwrap_or_else(|_| {
                log::error!("Worker pool channel closed unexpectedly");
                Err(TerminationReason::Other(
                    "Worker pool channel closed".to_string(),
                ))
            })
    }
}

/// Execute a task in the worker pool without waiting for completion (fire-and-forget).
///
/// Used when the task handles its own response channels.
pub fn execute_task(config: TaskExecutionConfig) {
    let components = match prepare_task_components(&config) {
        Some(c) => c,
        None => return,
    };

    let limits = config.limits;
    let task = config.task;
    let external_timeout_ms = config.external_timeout_ms;
    let permit = config.permit;

    WORKER_POOL.spawn(move || async move {
        // Wrap permit to automatically notify drain monitor on drop
        let _permit = TaskPermit::new(permit);

        let mut worker = match create_worker(
            components.script,
            limits,
            components.ops,
            &components.code_type,
        )
        .await
        {
            Ok(w) => w,
            Err(err) => {
                log::error!("Failed to create worker: {err:?}");
                components.log_handler.flush();
                return;
            }
        };

        let result = run_task_with_timeout_worker(&mut worker, task, external_timeout_ms).await;

        match result {
            Ok(()) => log::debug!("Task completed successfully"),
            Err(reason) => log::error!("Task failed: {:?}", reason),
        }

        // CRITICAL: Flush logs before worker is dropped to prevent log loss
        components.log_handler.flush();

        // TaskPermit is automatically dropped here, releasing the semaphore
        // and notifying the drain monitor
    });
}
