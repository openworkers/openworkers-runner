use std::ops::Deref;
use std::time::Duration;

use bytes::Bytes;
use openworkers_runtime::FetchInit;
use openworkers_runtime::RuntimeLimits;
use openworkers_runtime::Script;
use openworkers_runtime::Task;
use openworkers_runtime::TerminationReason;
use openworkers_runtime::Worker;
use tokio::sync::OwnedSemaphorePermit;

use crate::store::WorkerData;
use crate::worker_pool::WORKER_POOL;

type ResTx = tokio::sync::oneshot::Sender<http_v02::Response<Bytes>>;
type TerminationTx = tokio::sync::oneshot::Sender<TerminationReason>;

// Default timeout for fetch events
const FETCH_TIMEOUT_MS: u64 = 64_000; // 64 seconds

pub fn run_fetch(
    worker: WorkerData,
    req: http_v02::Request<Bytes>,
    res_tx: ResTx,
    termination_tx: TerminationTx,
    global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
    permit: OwnedSemaphorePermit,
) {
    let (log_tx, log_handler) = crate::log::create_log_handler(worker.id.clone(), global_log_tx);

    let code = match crate::transform::parse_worker_code(&worker) {
        Ok(code) => code,
        Err(e) => {
            log::error!("Failed to parse worker code: {}", e);
            res_tx
                .send(
                    http_v02::Response::builder()
                        .status(500)
                        .body(format!("Failed to parse worker code: {}", e).into())
                        .unwrap(),
                )
                .ok(); // Ignore send error
            termination_tx.send(TerminationReason::InitializationError).ok();
            return;
        }
    };

    let script = Script {
        code,
        env: match worker.env {
            Some(env) => Some(env.deref().to_owned()),
            None => None,
        },
    };

    // Use the global worker pool instead of spawning a new thread
    WORKER_POOL.spawn_pinned(move || async move {
        // Keep the permit alive for the entire worker execution
        // It will be automatically released when this async block completes
        let _permit = permit;

        log::debug!("create worker");

        let limits = RuntimeLimits {
            max_cpu_time_ms: 100,           // 100ms CPU time for fetch tasks
            max_wall_clock_time_ms: 60_000, // 60s total time for fetch tasks
            ..Default::default()
        };

        let mut worker = match Worker::new(script, Some(log_tx), Some(limits)).await {
            Ok(worker) => worker,
            Err(err) => {
                log::error!("failed to create worker: {err}");
                res_tx
                    .send(
                        http_v02::Response::builder()
                            .status(500)
                            .body(format!("failed to create worker: {err}").into())
                            .unwrap(),
                    )
                    .unwrap();

                return;
            }
        };

        let task = Task::Fetch(Some(FetchInit::new(req, res_tx)));

        log::debug!("exec fetch task with {}ms timeout", FETCH_TIMEOUT_MS);

        // Wrap execution with timeout
        let timeout_duration = Duration::from_millis(FETCH_TIMEOUT_MS);
        let termination_reason = match tokio::time::timeout(timeout_duration, worker.exec(task)).await {
            Ok(Ok(reason)) => {
                log::debug!("worker exec completed: reason={:?}", reason);
                reason
            }
            Ok(Err(err)) => {
                log::error!("worker exec error: {err}");
                TerminationReason::Exception
            }
            Err(_) => {
                log::error!("worker exec timeout after {}ms (outer timeout)", FETCH_TIMEOUT_MS);
                TerminationReason::WallClockTimeout
            }
        };

        // Send termination reason back to the main thread
        let _ = termination_tx.send(termination_reason);

        // CRITICAL: Flush logs before worker is dropped to prevent log loss
        log_handler.flush();

        // Permit is automatically released here when _permit goes out of scope
    });
}
