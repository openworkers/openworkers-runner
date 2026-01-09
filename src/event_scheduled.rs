use std::sync::Arc;

use crate::ops::{DbPool, RunnerOperations};
use crate::store::{self, WorkerWithBindings};
use crate::worker::{create_worker, prepare_script};
use crate::worker_pool::{TaskPermit, WORKER_POOL};

use openworkers_core::{RuntimeLimits, ScheduledInit, Task, TerminationReason};

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
    let script = match prepare_script(&worker_data) {
        Ok(s) => s,
        Err(err) => {
            log::error!("Failed to prepare script for scheduled task: {err:?}");
            return;
        }
    };

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

    let worker_id = worker_data.id.clone();
    let bindings = worker_data.bindings.clone();
    let code_type = worker_data.code_type.clone();
    let (log_tx, log_handler) = crate::log::create_log_handler(worker_id.clone(), global_log_tx);

    // Use the sequential worker pool - ensures ONE V8 isolate per thread at a time
    WORKER_POOL.spawn(move || async move {
        // Wrap permit to automatically notify drain monitor on drop
        let _permit = TaskPermit::new(permit);
        log::debug!("create worker");

        let limits = RuntimeLimits {
            max_cpu_time_ms: 100,           // 100ms CPU time for scheduled tasks
            max_wall_clock_time_ms: 60_000, // 60s total time for scheduled tasks
            ..Default::default()
        };

        // Create operations handle (includes logging, bindings, and db pool)
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
                log::error!("failed to create scheduled worker: {err:?}");
                log_handler.flush();
                return;
            }
        };

        // Create the oneshot channel INSIDE the async block so the receiver stays alive
        let (res_tx, res_rx) = tokio::sync::oneshot::channel::<()>();
        let task = Task::Scheduled(Some(ScheduledInit::new(res_tx, data.scheduled_time)));

        log::debug!("exec scheduled task");

        match worker.exec(task).await {
            Ok(()) => {
                log::debug!("scheduled task completed successfully");
                // Wait for the scheduled event to complete
                match res_rx.await {
                    Ok(()) => log::debug!("scheduled task responded"),
                    Err(err) => log::error!("scheduled task response error: {err}"),
                }
            }
            Err(reason) => match reason {
                TerminationReason::CpuTimeLimit => {
                    log::warn!("scheduled task terminated: CPU time limit exceeded");
                }
                TerminationReason::WallClockTimeout => {
                    log::warn!("scheduled task terminated: wall-clock timeout");
                }
                TerminationReason::MemoryLimit => {
                    log::warn!("scheduled task terminated: memory limit exceeded");
                }
                TerminationReason::Exception(msg) => {
                    log::error!("scheduled task terminated: uncaught exception: {}", msg);
                }
                _ => {
                    log::error!("scheduled task terminated: {:?}", reason);
                }
            },
        }

        // CRITICAL: Flush logs before worker is dropped to prevent log loss
        log_handler.flush();

        // TaskPermit is automatically dropped here, releasing the semaphore
        // and notifying the drain monitor
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

                let worker_id = store::WorkerIdentifier::Id(data.worker_id.clone());
                let worker_data = match store::get_worker_with_bindings(&mut conn, worker_id).await
                {
                    Some(w) => w,
                    None => {
                        log::error!("worker not found: {:?}", data.worker_id);
                        continue;
                    }
                };

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
