use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::LocalSet;

use crate::ops::{DbPool, RunnerOperations};
use crate::runtime::{
    FetchInit, HttpRequest, HttpResponse, ResponseBody, ResponseSender, RuntimeLimits, Script,
    Task, TerminationReason, Worker,
};
use crate::store::{WorkerWithBindings, bindings_to_infos};

type TerminationTx = tokio::sync::oneshot::Sender<Result<(), TerminationReason>>;

// Default timeout for fetch events
const FETCH_TIMEOUT_MS: u64 = 64_000; // 64 seconds

pub fn run_fetch(
    worker: WorkerWithBindings,
    req: HttpRequest,
    res_tx: ResponseSender,
    termination_tx: TerminationTx,
    global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
    permit: OwnedSemaphorePermit,
    db_pool: DbPool,
) {
    let worker_id = worker.id.clone();
    let (log_tx, log_handler) = crate::log::create_log_handler(worker_id.clone(), global_log_tx);

    let code = match crate::transform::parse_worker_code_str(&worker.script, &worker.language) {
        Ok(code) => code,
        Err(e) => {
            log::error!("Failed to parse worker code: {}", e);
            res_tx
                .send(HttpResponse {
                    status: 500,
                    headers: vec![],
                    body: ResponseBody::Bytes(format!("Failed to parse worker code: {}", e).into()),
                })
                .ok();
            termination_tx
                .send(Err(TerminationReason::InitializationError(format!(
                    "Failed to parse worker code: {}",
                    e
                ))))
                .ok();
            return;
        }
    };

    // Convert bindings to BindingInfo for Script (names + types only)
    let binding_infos = bindings_to_infos(&worker.bindings);

    // Clone bindings for RunnerOperations (full configs)
    let bindings_for_ops = worker.bindings.clone();

    let script = Script {
        code,
        env: if worker.env.is_empty() {
            None
        } else {
            Some(worker.env.clone())
        },
        bindings: binding_infos,
    };

    // Spawn a dedicated thread for each worker to avoid V8 isolate interleaving.
    // LocalPoolHandle's spawn_pinned can interleave multiple workers on the same thread,
    // which causes "Cannot exit non-entered context" errors in V8.
    std::thread::spawn(move || {
        // Create a dedicated single-threaded tokio runtime for this worker
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime for worker");

        // Use LocalSet to support spawn_local (needed for event_loop and streaming)
        let local = LocalSet::new();

        local.block_on(&rt, async move {
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
                    .with_bindings(bindings_for_ops)
                    .with_db_pool(db_pool),
            );

            let mut worker = match Worker::new_with_ops(script, Some(limits), ops).await {
                Ok(worker) => worker,
                Err(err) => {
                    log::error!("failed to create worker: {err}");
                    res_tx
                        .send(HttpResponse {
                            status: 500,
                            headers: vec![],
                            body: ResponseBody::Bytes(
                                format!("failed to create worker: {err}").into(),
                            ),
                        })
                        .unwrap();

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
    });
}
