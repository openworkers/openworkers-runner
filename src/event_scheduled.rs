use crate::ops::DbPool;
use crate::store::{self, WorkerWithBindings};
use crate::task_executor::{self, TaskExecutionConfig};
use crate::worker::prepare_script;

use openworkers_core::Event;

use serde::Deserialize;
use serde::Serialize;

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
    worker_data: WorkerWithBindings,
    db_pool: DbPool,
    global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
) {
    // Parse script before spawning (fail fast)
    if let Err(err) = prepare_script(&worker_data) {
        log::error!("Failed to prepare script for scheduled task: {err:?}");
        return;
    }

    // Try to acquire a worker slot
    let permit = match crate::worker_pool::WORKER_SEMAPHORE
        .clone()
        .try_acquire_owned()
    {
        Ok(permit) => permit,
        Err(_) => {
            log::warn!(
                "worker pool saturated, skipping scheduled task for worker: {}",
                data.worker_id
            );
            return;
        }
    };

    // Execute task using common executor (fire-and-forget)
    // Note: scheduled tasks create their own response channel internally
    tokio::spawn(async move {
        // Create task event with schedule source
        let task_id = format!("scheduled-{}", data.id);
        let (event, res_rx) = Event::from_schedule(task_id, data.scheduled_time);

        let config = TaskExecutionConfig {
            worker_data,
            permit,
            task: event,
            db_pool,
            global_log_tx,
            limits: task_executor::TaskExecutionConfig::default_limits(),
            external_timeout_ms: None, // No external timeout for scheduled tasks
        };

        // Execute the task
        let result = task_executor::execute_task_await(config).await;

        // Log the execution result
        match result {
            Ok(()) => {
                log::debug!("scheduled task exec completed successfully");
                // Wait for the task event handler to respond
                match res_rx.await {
                    Ok(task_result) => {
                        if task_result.success {
                            log::debug!("scheduled task responded successfully");
                        } else {
                            log::error!(
                                "scheduled task failed: {}",
                                task_result.error.unwrap_or_default()
                            );
                        }
                    }
                    Err(err) => log::error!("scheduled task response error: {err}"),
                }
            }
            Err(reason) => {
                log::error!("scheduled task terminated: {:?}", reason);
            }
        }
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

            let nc = crate::nats::nats_connect().await;
            let mut sub = nc
                .queue_subscribe("scheduled".to_string(), "runner".to_string())
                .await
                .expect("failed to subscribe to scheduled");

            log::debug!("listening for scheduled tasks");

            let notify = crate::worker_pool::TASK_COMPLETION_NOTIFY.clone();

            loop {
                // Listen to both NATS messages and task completion events
                // This allows immediate reaction to draining state changes:
                // - If draining starts while waiting for NATS message, we stop listening immediately
                // - Messages stay in NATS queue for other runners to process
                // - No messages are lost or dequeued during shutdown
                let msg = tokio::select! {
                    Some(msg) = sub.next() => msg,
                    _ = notify.notified() => {
                        // Check if draining and stop listening
                        if crate::worker_pool::is_draining() {
                            log::info!("Runner is draining - stopping scheduled task listener");
                            break;
                        }
                        continue;
                    }
                };

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

                // Acquire connection per-task to avoid holding it across iterations
                let mut conn = match db.acquire().await {
                    Ok(c) => c,
                    Err(err) => {
                        log::error!("Failed to acquire database connection: {}", err);
                        continue;
                    }
                };

                let worker_id = store::WorkerIdentifier::Id(data.worker_id.clone());
                let worker_data = match store::get_worker_with_bindings(&mut conn, worker_id).await
                {
                    Some(w) => w,
                    None => {
                        log::error!(
                            "worker not found: {}",
                            crate::utils::short_id(&data.worker_id)
                        );
                        continue;
                    }
                };

                // Connection is dropped here, returned to pool

                run_scheduled(data, worker_data, db.clone(), global_log_tx.clone());
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
