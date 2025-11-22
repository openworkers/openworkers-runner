use std::ops::Deref;

use openworkers_runtime::RuntimeLimits;
use openworkers_runtime::ScheduledInit;
use openworkers_runtime::Script;
use openworkers_runtime::Task;
use openworkers_runtime::Worker;

use serde::Deserialize;
use serde::Serialize;

use crate::store;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledData {
    pub id: String,
    pub cron: String,
    pub scheduled_time: u64,
    pub worker_id: String,
}

fn run_scheduled(
    data: ScheduledData,
    script: Script,
    global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
) {
    let (res_tx, res_rx) = tokio::sync::oneshot::channel::<()>();

    let task = Task::Scheduled(Some(ScheduledInit::new(res_tx, data.scheduled_time)));

    let log_tx = crate::log::create_log_handler(data.worker_id, global_log_tx);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let local = tokio::task::LocalSet::new();

        local.spawn_local(async move {
            log::debug!("create worker");

            let limits = RuntimeLimits {
                max_cpu_time_ms: 100,           // 100ms CPU time for scheduled tasks
                max_wall_clock_time_ms: 60_000, // 60s total time for scheduled tasks
                ..Default::default()
            };

            let mut worker = Worker::new(script, Some(log_tx), Some(limits))
                .await
                .unwrap();

            log::debug!("exec scheduled task");
            match worker.exec(task).await {
                Ok(()) => log::debug!("exec completed"),
                Err(err) => log::error!("exec did not complete: {err}"),
            }
        });

        log::debug!("scheduled task listener started");

        match local.block_on(&rt, async { res_rx.await }) {
            Ok(()) => {}
            Err(err) => log::error!("failed to wait for end: {err}"),
        }

        log::debug!("scheduled task listener stopped");
    });
}

pub fn handle_scheduled(
    db: sqlx::Pool<sqlx::Postgres>,
    global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
) {
    std::thread::spawn(move || {
        let local = tokio::task::LocalSet::new();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let handle = local.spawn_local(async move {
            use futures::StreamExt;

            // Acquire a database connection from the pool.
            let mut conn: sqlx::pool::PoolConnection<sqlx::Postgres> = match db.acquire().await {
                Ok(db) => db,
                Err(err) => {
                    log::error!("Failed to acquire a database connection: {}", err);
                    return;
                }
            };

            let nc = crate::nats::nats_connect().await;
            let mut sub = nc
                .queue_subscribe("scheduled".to_string(), "runner".to_string())
                .await
                .expect("failed to subscribe to scheduled");

            log::debug!("listening for scheduled tasks");

            while let Some(msg) = sub.next().await {
                log::debug!("scheduled task received: {:?}", msg);

                let data: ScheduledData =
                    match serde_json::from_slice::<ScheduledData>(&msg.payload) {
                        Ok(msg) => msg,
                        Err(err) => {
                            log::error!("failed to parse scheduled task: {:?}", err);
                            continue;
                        }
                    };

                log::debug!("scheduled task parsed: {:?}", data);

                let worker_id = store::WorkerIdentifier::Id(data.worker_id.clone());
                let worker = match store::get_worker(&mut conn, worker_id).await {
                    Some(worker) => worker,
                    None => {
                        log::error!("worker not found: {:?}", data.worker_id);
                        continue;
                    }
                };

                let script = Script {
                    code: crate::transform::parse_worker_code(&worker),
                    env: match worker.env {
                        Some(env) => Some(env.deref().to_owned()),
                        None => None,
                    },
                };

                run_scheduled(data, script, global_log_tx.clone());
            }

            log::debug!("scheduled task listener stopped");
        });

        log::debug!("subscribing to scheduled {:?}", handle);

        match local.block_on(&rt, handle) {
            Ok(()) => {}
            Err(err) => log::error!("failed to wait for end: {err}"),
        }
    });
}
