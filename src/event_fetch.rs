use crate::ops::DbPool;
use crate::store::WorkerWithBindings;
use crate::task_executor::{self, TaskExecutionConfig};
use crate::worker::prepare_script;

use openworkers_core::{
    Event, FetchInit, HttpRequest, HttpResponse, ResponseBody, ResponseSender, TerminationReason,
};
use tracing::Instrument;

type TerminationTx = tokio::sync::oneshot::Sender<Result<(), TerminationReason>>;

#[allow(clippy::too_many_arguments)]
pub fn run_fetch(
    worker_data: WorkerWithBindings,
    req: HttpRequest,
    res_tx: ResponseSender,
    termination_tx: TerminationTx,
    global_log_tx: std::sync::mpsc::Sender<crate::log::LogMessage>,
    permit: tokio::sync::OwnedSemaphorePermit,
    db_pool: DbPool,
    wall_clock_timeout_ms: u64,
    span: tracing::Span,
) {
    // Parse script before spawning (fail fast)
    if let Err(err) = prepare_script(&worker_data) {
        tracing::error!("Failed to prepare script: {err:?}");
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

    // Create the task
    let task = Event::Fetch(Some(FetchInit::new(req, res_tx)));

    // Build config for task executor
    let config = TaskExecutionConfig {
        worker_data,
        permit,
        task,
        db_pool,
        global_log_tx,
        limits: task_executor::TaskExecutionConfig::default_limits(),
        external_timeout_ms: Some(wall_clock_timeout_ms),
        span: span.clone(),
    };

    // Spawn async task to execute and send result back
    // Instrument with span to propagate trace context to worker pool thread
    tokio::spawn(
        async move {
            let result = task_executor::execute_task_await(config).await;
            let _ = termination_tx.send(result);
        }
        .instrument(span),
    );
}
