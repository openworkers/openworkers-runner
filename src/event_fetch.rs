use std::thread::JoinHandle;

use bytes::Bytes;
use openworkers_runtime::FetchInit;
use openworkers_runtime::Script;
use openworkers_runtime::Task;
use openworkers_runtime::Worker;

use crate::store::WorkerData;

type ResTx = tokio::sync::oneshot::Sender<http_v02::Response<Bytes>>;

pub fn run_fetch(
    worker: WorkerData,
    req: http_v02::Request<Bytes>,
    res_tx: ResTx,
) -> JoinHandle<()> {
    let log_tx = crate::log::create_log_handler(worker.id.clone());

    let script = Script {
        specifier: openworkers_runtime::module_url("script.js"),
        code: Some(crate::transform::parse_worker_code(&worker)),
        env: match worker.env {
            Some(env) => Some(env.encode_to_string()),
            None => None,
        },
    };

    std::thread::spawn(move || {
        let local = tokio::task::LocalSet::new();

        let tasks = local.spawn_local(async move {
            log::debug!("create worker");
            let mut worker = match Worker::new(script, Some(log_tx)).await {
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

            log::debug!("exec fetch task");
            match worker.exec(task).await {
                Ok(()) => log::debug!("exec completed"),
                Err(err) => log::error!("exec did not complete: {err}"),
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
