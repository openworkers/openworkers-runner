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

/// Handler for a scheduled task that executes within the span context
async fn handle_scheduled_task(
    span: tracing::Span,
    task_id: String,
    data: ScheduledData,
    db: sqlx::Pool<sqlx::Postgres>,
    global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
) {
    // Start metrics timer
    #[cfg_attr(not(feature = "telemetry"), allow(unused_mut))]
    let mut metrics_timer = crate::metrics::MetricsTimer::new();

    // Acquire connection per-task to avoid holding it across iterations
    let mut conn = match db.acquire().await {
        Ok(c) => c,
        Err(err) => {
            tracing::error!("Failed to acquire database connection: {}", err);
            return;
        }
    };

    let worker_id = store::WorkerIdentifier::Id(data.worker_id.clone());
    let worker_data = match store::get_worker_with_bindings(&mut conn, worker_id).await {
        Some(w) => w,
        None => {
            tracing::error!(
                "worker not found: {}",
                crate::utils::short_id(&data.worker_id)
            );
            return;
        }
    };

    // Record worker info in span now that we have it
    span.record("worker_id", tracing::field::display(&worker_data.id));
    if let Some(name) = &worker_data.name {
        span.record("worker_name", tracing::field::display(name));
    }
    span.record("user_id", tracing::field::display(&worker_data.user_id));

    // Add metrics labels
    #[cfg(feature = "telemetry")]
    {
        use opentelemetry::KeyValue;
        metrics_timer = metrics_timer.with_labels(vec![
            KeyValue::new("worker_id", worker_data.id.clone()),
            KeyValue::new("user_id", worker_data.user_id.clone()),
            KeyValue::new("cron", data.cron.clone()),
        ]);
    }

    // Connection is dropped here, returned to pool
    drop(conn);

    run_scheduled(
        task_id,
        data,
        worker_data,
        db,
        global_log_tx,
        span,
        metrics_timer,
    );
}

fn run_scheduled(
    task_id: String,
    data: ScheduledData,
    worker_data: WorkerWithBindings,
    db_pool: DbPool,
    global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
    span: tracing::Span,
    mut metrics_timer: crate::metrics::MetricsTimer,
) {
    // Parse script before spawning (fail fast)
    if let Err(err) = prepare_script(&worker_data) {
        tracing::error!("Failed to prepare script for scheduled task: {err:?}");
        return;
    }

    // Try to acquire a worker slot
    let permit = match crate::worker_pool::WORKER_SEMAPHORE
        .clone()
        .try_acquire_owned()
    {
        Ok(permit) => permit,
        Err(_) => {
            tracing::warn!(
                "worker pool saturated, skipping scheduled task for worker: {}",
                data.worker_id
            );
            return;
        }
    };

    // Mark worker spawned (for queue time metric)
    metrics_timer.mark_worker_spawned();

    // Execute task using common executor (fire-and-forget)
    // Note: scheduled tasks create their own response channel internally
    // Span is inherited from parent context via .instrument()
    use tracing::Instrument;

    tokio::spawn(
        async move {
            // Create task event with schedule source
            let (event, res_rx) = Event::from_schedule(task_id, data.scheduled_time);

            let config = TaskExecutionConfig {
                worker_data,
                permit,
                task: event,
                db_pool,
                global_log_tx,
                limits: task_executor::TaskExecutionConfig::default_limits(),
                external_timeout_ms: None, // No external timeout for scheduled tasks
                span: tracing::Span::current(),
            };

            // Execute the task
            let result = task_executor::execute_task_await(config).await;

            // Log the execution result and track success
            let success = match result {
                Ok(()) => {
                    tracing::debug!("scheduled task exec completed successfully");
                    // Wait for the task event handler to respond
                    match res_rx.await {
                        Ok(task_result) => {
                            if task_result.success {
                                tracing::debug!("scheduled task responded successfully");
                                true
                            } else {
                                tracing::error!(
                                    "scheduled task failed: {}",
                                    task_result.error.unwrap_or_default()
                                );
                                false
                            }
                        }
                        Err(err) => {
                            tracing::error!("scheduled task response error: {err}");
                            false
                        }
                    }
                }
                Err(reason) => {
                    tracing::error!("scheduled task terminated: {:?}", reason);
                    false
                }
            };

            // Record metrics
            metrics_timer.record_scheduled_task(success);
        }
        .instrument(span),
    );
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

            tracing::debug!("listening for scheduled tasks");

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
                            tracing::info!("Runner is draining - stopping scheduled task listener");
                            break;
                        }
                        continue;
                    }
                };

                tracing::debug!("scheduled task received: {:?}", msg);

                let data: ScheduledData =
                    match serde_json::from_slice::<ScheduledData>(&msg.payload) {
                        Ok(msg) => msg,
                        Err(err) => {
                            tracing::error!("failed to parse scheduled task: {:?}", err);
                            continue;
                        }
                    };

                tracing::debug!("scheduled task parsed: {:?}", data);

                // Create span for this scheduled task early
                let task_id = format!("scheduled-{}", data.id);
                let cron = format!("\"{}\"", data.cron);
                let span = tracing::info_span!(
                    "scheduled_task",
                    task_id = %task_id,
                    cron = %cron,
                    worker_id = tracing::field::Empty,
                    worker_name = tracing::field::Empty,
                    user_id = tracing::field::Empty,
                );

                // Use Instrument trait for async operations
                use tracing::Instrument;

                // Execute the task handler within the span context
                handle_scheduled_task(
                    span.clone(),
                    task_id,
                    data,
                    db.clone(),
                    global_log_tx.clone(),
                )
                .instrument(span)
                .await;
            }

            tracing::debug!("scheduled task listener stopped");
        });

        tracing::debug!("subscribing to scheduled {:?}", handle);

        match local.block_on(&rt, handle) {
            Ok(()) => {}
            Err(err) => tracing::error!("failed to wait for end: {err}"),
        }
    });
}
