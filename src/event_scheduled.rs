use std::time::Duration;

use crate::metrics::Outcome;
use crate::ops::DbPool;
use crate::store::{self, WorkerWithBindings};
use crate::task_executor::{self, TaskExecutionConfig};
use crate::worker::prepare_worker;

use openworkers_core::Event;
use openworkers_core::TaskSource;

/// How long a scheduled task may hold a worker slot. A cron fires again whatever
/// happens, so one that never ends would take a slot out of the pool for good.
const SCHEDULED_TASK_TIMEOUT_MS: u64 = 60_000;

#[derive(Debug)]
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
    db_internal: sqlx::Pool<sqlx::Postgres>,
    db_worker: sqlx::Pool<sqlx::Postgres>,
    log_sink: crate::log::LogSink,
) {
    // Start metrics timer
    #[cfg_attr(not(feature = "telemetry"), allow(unused_mut))]
    let mut metrics_timer = crate::metrics::MetricsTimer::new();

    // Acquire connection from internal pool (worker lookup is an internal query)
    let mut conn = match db_internal.acquire().await {
        Ok(c) => c,
        Err(err) => {
            tracing::error!("Failed to acquire database connection: {}", err);
            metrics_timer.record_scheduled_task(Outcome::Failed("db_unavailable"));
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
            metrics_timer.record_scheduled_task(Outcome::Failed("worker_not_found"));
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

    // Connection is dropped here, returned to internal pool
    drop(conn);

    // Worker execution uses the worker pool for binding operations
    run_scheduled(
        task_id,
        data,
        worker_data,
        db_worker,
        log_sink,
        span,
        metrics_timer,
    );
}

fn run_scheduled(
    task_id: String,
    data: ScheduledData,
    worker_data: WorkerWithBindings,
    db_pool: DbPool,
    log_sink: crate::log::LogSink,
    span: tracing::Span,
    mut metrics_timer: crate::metrics::MetricsTimer,
) {
    let limits = task_executor::TaskExecutionConfig::default_limits();

    // Prepared once, before spawning: a script that does not parse fails fast
    let prepared = match prepare_worker(&worker_data, &limits) {
        Ok(prepared) => prepared,
        Err(err) => {
            tracing::error!("Failed to prepare script for scheduled task: {err:?}");
            metrics_timer.record_scheduled_task(Outcome::Failed("script_invalid"));
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
            tracing::warn!(
                "worker pool saturated, skipping scheduled task for worker: {}",
                data.worker_id
            );
            metrics_timer.record_scheduled_task(Outcome::Failed("overloaded"));
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
            let source = TaskSource::Schedule {
                time: data.scheduled_time,
                cron: Some(data.cron),
            };

            let (event, res_rx) = Event::task(task_id, None, Some(source), 1);

            let config = TaskExecutionConfig {
                worker_data,
                permit,
                task: event,
                db_pool,
                log_sink,
                limits,
                // A task that never ends holds its pool permit for the life of
                // the process. Nothing else releases it, and the pool climbed to
                // saturation over sixteen hours on the back of that.
                external_timeout_ms: Some(SCHEDULED_TASK_TIMEOUT_MS),
                abort: Some(tokio_util::sync::CancellationToken::new()),
                span: tracing::Span::current(),
            };

            // Execute the task
            let result = task_executor::execute_task_await(config, prepared).await;

            // Log the execution result and track the outcome
            let outcome = match result {
                Ok(()) => {
                    tracing::debug!("scheduled task exec completed successfully");

                    // A handler that settles nothing would leave this waiting as
                    // long as the process lives, permit in hand.
                    let answered = tokio::time::timeout(
                        Duration::from_millis(SCHEDULED_TASK_TIMEOUT_MS),
                        res_rx,
                    )
                    .await;

                    let Ok(answered) = answered else {
                        tracing::error!(
                            "scheduled task produced no result within {}ms",
                            SCHEDULED_TASK_TIMEOUT_MS
                        );
                        metrics_timer.record_scheduled_task(Outcome::Failed("no_result"));

                        return;
                    };

                    match answered {
                        Ok(task_result) => {
                            if task_result.success {
                                tracing::debug!("scheduled task responded successfully");
                                Outcome::Success
                            } else {
                                tracing::error!(
                                    "scheduled task failed: {}",
                                    task_result.error.unwrap_or_default()
                                );
                                Outcome::Failed("task_error")
                            }
                        }
                        Err(err) => {
                            tracing::error!("scheduled task response error: {err}");
                            Outcome::Failed("no_response")
                        }
                    }
                }
                Err(reason) => {
                    tracing::error!("scheduled task terminated: {:?}", reason);
                    Outcome::terminated(&reason)
                }
            };

            // Record metrics
            metrics_timer.record_scheduled_task(outcome);
        }
        .instrument(span),
    );
}

pub async fn dispatch(
    data: ScheduledData,
    db_internal: sqlx::PgPool,
    db_worker: sqlx::PgPool,
    log_sink: crate::log::LogSink,
) {
    if crate::worker_pool::is_draining() {
        return;
    }

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

    use tracing::Instrument;

    handle_scheduled_task(
        span.clone(),
        task_id,
        data,
        db_internal,
        db_worker,
        log_sink,
    )
    .instrument(span)
    .await;
}
