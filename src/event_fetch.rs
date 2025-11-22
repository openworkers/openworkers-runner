use std::ops::Deref;
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::Bytes;
use openworkers_runtime::FetchInit;
use openworkers_runtime::RuntimeLimits;
use openworkers_runtime::Script;
use openworkers_runtime::Task;
use openworkers_runtime::Worker;

use crate::store::WorkerData;

type ResTx = tokio::sync::oneshot::Sender<http_v02::Response<Bytes>>;

// Default timeout for fetch events
const FETCH_TIMEOUT_MS: u64 = 64_000; // 64 seconds

pub fn run_fetch(
    worker: WorkerData,
    req: http_v02::Request<Bytes>,
    res_tx: ResTx,
    global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
) -> JoinHandle<()> {
    let log_tx = crate::log::create_log_handler(worker.id.clone(), global_log_tx);

    let script = Script {
        code: crate::transform::parse_worker_code(&worker),
        env: match worker.env {
            Some(env) => Some(env.deref().to_owned()),
            None => None,
        },
    };

    std::thread::spawn(move || {
        let local = tokio::task::LocalSet::new();

        let tasks = local.spawn_local(async move {
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
            match tokio::time::timeout(timeout_duration, worker.exec(task)).await {
                Ok(Ok(())) => log::debug!("exec completed"),
                Ok(Err(err)) => log::error!("exec did not complete: {err}"),
                Err(_) => {
                    log::error!("exec timeout after {}ms", FETCH_TIMEOUT_MS);
                    // Note: Worker may have already sent a response via FetchInit
                    // If no response was sent, res_tx will be dropped and client gets an error
                }
            }
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        match local.block_on(&rt, tasks) {
            Ok(()) => {}
            Err(err) => log::error!("failed to wait for end: {err}"),
        }
    })
}
