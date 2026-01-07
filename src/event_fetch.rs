use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;

use crate::ops::{DbPool, RunnerOperations};
use crate::store::WorkerWithBindings;
use crate::worker::{create_worker, prepare_script};
use crate::worker_pool::WORKER_POOL;

use openworkers_core::{
    FetchInit, HttpRequest, HttpResponse, ResponseBody, ResponseSender, RuntimeLimits, Task,
    TerminationReason,
};

type TerminationTx = tokio::sync::oneshot::Sender<Result<(), TerminationReason>>;

// Default timeout for fetch events
const FETCH_TIMEOUT_MS: u64 = 64_000; // 64 seconds

pub fn run_fetch(
    worker_data: WorkerWithBindings,
    req: HttpRequest,
    res_tx: ResponseSender,
    termination_tx: TerminationTx,
    global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
    permit: OwnedSemaphorePermit,
    db_pool: DbPool,
) {
    let worker_id = worker_data.id.clone();
    let bindings = worker_data.bindings.clone();
    let code_type = worker_data.code_type.clone();

    // Parse script before spawning (fail fast)
    let script = match prepare_script(&worker_data) {
        Ok(s) => s,
        Err(err) => {
            log::error!("Failed to prepare script: {err:?}");
            res_tx
                .send(HttpResponse {
                    status: 500,
                    headers: vec![],
                    body: ResponseBody::Bytes(format!("Failed to prepare script: {err:?}").into()),
                })
                .ok();
            termination_tx.send(Err(err)).ok();
            return;
        }
    };

    let (log_tx, log_handler) = crate::log::create_log_handler(worker_id.clone(), global_log_tx);

    // Use the sequential worker pool - ensures ONE V8 isolate per thread at a time
    WORKER_POOL.spawn(move || async move {
        // Keep the permit alive for the entire worker execution
        // It will be automatically released when this async block completes
        let _permit = permit;

        log::debug!("create worker");

        let limits = RuntimeLimits {
            max_cpu_time_ms: 100,           // 100ms CPU time for fetch tasks
            max_wall_clock_time_ms: 60_000, // 60s total time for fetch tasks
            ..Default::default()
        };

        // Create operations handle for fetch delegation (includes logging and bindings)
        let ops = Arc::new(
            RunnerOperations::new()
                .with_worker_id(worker_id)
                .with_log_tx(log_tx)
                .with_bindings(bindings)
                .with_db_pool(db_pool),
        );

        // Create worker
        let mut worker = match create_worker(script, limits, ops, &code_type).await {
            Ok(w) => w,
            Err(err) => {
                log::error!("failed to create worker: {err:?}");
                res_tx
                    .send(HttpResponse {
                        status: 500,
                        headers: vec![],
                        body: ResponseBody::Bytes(
                            format!("failed to create worker: {err:?}").into(),
                        ),
                    })
                    .ok();
                termination_tx.send(Err(err)).ok();
                log_handler.flush();
                return;
            }
        };

        let task = Task::Fetch(Some(FetchInit::new(req, res_tx)));
        log::debug!("exec fetch task with {}ms timeout", FETCH_TIMEOUT_MS);

        // Wrap execution with timeout
        let timeout_duration = Duration::from_millis(FETCH_TIMEOUT_MS);
        let result = match tokio::time::timeout(timeout_duration, worker.exec(task)).await {
            Ok(result) => {
                log::debug!("worker exec completed: {:?}", result);
                result
            }
            Err(_) => {
                log::error!(
                    "worker exec timeout after {}ms (outer timeout)",
                    FETCH_TIMEOUT_MS
                );
                Err(TerminationReason::WallClockTimeout)
            }
        };

        // Send result back to the main thread
        let _ = termination_tx.send(result);

        // CRITICAL: Flush logs before worker is dropped to prevent log loss
        log_handler.flush();

        // Permit is automatically released here when _permit goes out of scope
    });
}
