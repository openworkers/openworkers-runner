use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;

use crate::ops::{DbPool, RunnerOperations};
use crate::store::WorkerWithBindings;
use crate::worker::{create_worker, prepare_script};
use crate::worker_pool::{TaskPermit, WORKER_POOL};

use openworkers_core::{RuntimeLimits, Task, TerminationReason};

// Default runtime limits for different task types
pub const DEFAULT_CPU_TIME_MS: u64 = 100;
pub const DEFAULT_WALL_CLOCK_TIME_MS: u64 = 60_000;

/// Configuration for executing a task
pub struct TaskExecutionConfig {
    pub worker_data: WorkerWithBindings,
    pub permit: OwnedSemaphorePermit,
    pub task: Task,
    pub db_pool: DbPool,
    pub global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
    pub limits: RuntimeLimits,
    pub external_timeout_ms: Option<u64>,
}

impl TaskExecutionConfig {
    /// Create default runtime limits
    pub fn default_limits() -> RuntimeLimits {
        RuntimeLimits {
            max_cpu_time_ms: DEFAULT_CPU_TIME_MS,
            max_wall_clock_time_ms: DEFAULT_WALL_CLOCK_TIME_MS,
            ..Default::default()
        }
    }
}

/// Execute a task in the worker pool and await its completion
///
/// This is the common execution path for all task types:
/// 1. Parse script (fail fast)
/// 2. Setup logging
/// 3. Create V8 isolate with runtime limits
/// 4. Execute task
/// 5. Flush logs
/// 6. Auto-release permit and notify drain monitor
///
/// Returns the task execution result
pub async fn execute_task_await(config: TaskExecutionConfig) -> Result<(), TerminationReason> {
    let worker_id = config.worker_data.id.clone();
    let code_type = config.worker_data.code_type.clone();
    let bindings = config.worker_data.bindings.clone();

    // Parse script before spawning (fail fast)
    let script = prepare_script(&config.worker_data).map_err(|err| {
        log::error!("Failed to prepare script: {err:?}");
        err
    })?;

    let (log_tx, log_handler) =
        crate::log::create_log_handler(worker_id.clone(), config.global_log_tx);

    // Execute in worker pool with spawn_await for result
    let result = WORKER_POOL
        .spawn_await(move || async move {
            // Wrap permit to automatically notify drain monitor on drop
            let _permit = TaskPermit::new(config.permit);

            log::debug!("Creating worker for task execution");

            // Create operations handle (includes logging, bindings, and db pool)
            let ops = Arc::new(
                RunnerOperations::new()
                    .with_worker_id(worker_id)
                    .with_log_tx(log_tx)
                    .with_bindings(bindings)
                    .with_db_pool(config.db_pool),
            );

            // Create worker
            let mut worker = create_worker(script, config.limits, ops, &code_type)
                .await
                .map_err(|err| {
                    log::error!("Failed to create worker: {err:?}");
                    err
                })?;

            // Execute task with optional external timeout
            let result = if let Some(timeout_ms) = config.external_timeout_ms {
                let timeout_duration = Duration::from_millis(timeout_ms);
                match tokio::time::timeout(timeout_duration, worker.exec(config.task)).await {
                    Ok(result) => {
                        log::debug!("Task execution completed: {:?}", result);
                        result
                    }
                    Err(_) => {
                        log::error!(
                            "Task execution timeout after {}ms (external timeout)",
                            timeout_ms
                        );
                        Err(TerminationReason::WallClockTimeout)
                    }
                }
            } else {
                worker.exec(config.task).await
            };

            // CRITICAL: Flush logs before worker is dropped to prevent log loss
            log_handler.flush();

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
        });

    result
}

/// Execute a task in the worker pool without waiting for completion (fire-and-forget)
///
/// This is used when the task handles its own response channels and we don't need
/// to wait for the result in the caller.
pub fn execute_task(config: TaskExecutionConfig) {
    let worker_id = config.worker_data.id.clone();
    let code_type = config.worker_data.code_type.clone();
    let bindings = config.worker_data.bindings.clone();

    // Parse script before spawning (fail fast)
    let script = match prepare_script(&config.worker_data) {
        Ok(s) => s,
        Err(err) => {
            log::error!("Failed to prepare script: {err:?}");
            return;
        }
    };

    let (log_tx, log_handler) =
        crate::log::create_log_handler(worker_id.clone(), config.global_log_tx);

    // Execute in worker pool (fire-and-forget)
    WORKER_POOL.spawn(move || async move {
        // Wrap permit to automatically notify drain monitor on drop
        let _permit = TaskPermit::new(config.permit);

        log::debug!("Creating worker for task execution");

        // Create operations handle (includes logging, bindings, and db pool)
        let ops = Arc::new(
            RunnerOperations::new()
                .with_worker_id(worker_id)
                .with_log_tx(log_tx)
                .with_bindings(bindings)
                .with_db_pool(config.db_pool),
        );

        // Create worker
        let mut worker = match create_worker(script, config.limits, ops, &code_type).await {
            Ok(w) => w,
            Err(err) => {
                log::error!("Failed to create worker: {err:?}");
                log_handler.flush();
                return;
            }
        };

        // Execute task with optional external timeout
        let result = if let Some(timeout_ms) = config.external_timeout_ms {
            let timeout_duration = Duration::from_millis(timeout_ms);
            match tokio::time::timeout(timeout_duration, worker.exec(config.task)).await {
                Ok(result) => {
                    log::debug!("Task execution completed: {:?}", result);
                    result
                }
                Err(_) => {
                    log::error!(
                        "Task execution timeout after {}ms (external timeout)",
                        timeout_ms
                    );
                    Err(TerminationReason::WallClockTimeout)
                }
            }
        } else {
            worker.exec(config.task).await
        };

        // Log the result
        match result {
            Ok(()) => log::debug!("Task completed successfully"),
            Err(reason) => log::error!("Task failed: {:?}", reason),
        }

        // CRITICAL: Flush logs before worker is dropped to prevent log loss
        log_handler.flush();

        // TaskPermit is automatically dropped here, releasing the semaphore
        // and notifying the drain monitor
    });
}
