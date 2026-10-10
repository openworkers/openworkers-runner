use crate::ops::DbPool;
use crate::store::WorkerWithBindings;
use crate::task_executor::{self, TaskExecutionConfig};
use crate::worker::prepare_worker;

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
    log_sink: crate::log::LogSink,
    permit: tokio::sync::OwnedSemaphorePermit,
    db_pool: DbPool,
    wall_clock_timeout_ms: u64,
    abort: tokio_util::sync::CancellationToken,
    span: tracing::Span,
) {
    let limits = task_executor::TaskExecutionConfig::default_limits();

    // Prepared once, before spawning: a script that does not parse fails fast
    let prepared = match prepare_worker(&worker_data, &limits) {
        Ok(prepared) => prepared,
        Err(err) => {
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
    };

    // Create the task
    let task = Event::Fetch(Some(FetchInit::new(req, res_tx)));

    // Build config for task executor
    let config = TaskExecutionConfig {
        worker_data,
        permit,
        task,
        db_pool,
        log_sink,
        limits,
        external_timeout_ms: Some(wall_clock_timeout_ms),
        abort: Some(abort),
        span: span.clone(),
    };

    // Spawn async task to execute and send result back
    // Instrument with span to propagate trace context to worker pool thread
    tokio::spawn(
        async move {
            let result = task_executor::execute_task_await(config, prepared).await;
            let _ = termination_tx.send(result);
        }
        .instrument(span),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{CodeType, WorkerSource};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// A script that does not parse is answered before anything is spawned,
    /// with the error the preparation gave.
    #[tokio::test]
    async fn a_script_that_does_not_parse_gets_a_500() {
        let worker_data = WorkerWithBindings {
            id: "worker".to_string(),
            name: None,
            user_id: "owner".to_string(),
            code: WorkerSource::Bytes(b"export default { fetch( }".to_vec()),
            code_type: CodeType::Javascript,
            version: 1,
            env: HashMap::new(),
            bindings: vec![],
            env_updated_at: None,
        };
        let request = HttpRequest {
            method: openworkers_core::HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: openworkers_core::RequestBody::None,
        };
        let (res_tx, res_rx) = tokio::sync::oneshot::channel();
        let (termination_tx, termination_rx) = tokio::sync::oneshot::channel();
        let (log_sink, _log_store) = crate::log::LogSink::new();
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap();
        // Never connects: the failure comes before any query
        let db_pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();

        run_fetch(
            worker_data,
            request,
            res_tx,
            termination_tx,
            log_sink,
            permit,
            db_pool,
            1_000,
            tokio_util::sync::CancellationToken::new(),
            tracing::Span::none(),
        );

        let response = res_rx.await.unwrap();
        let ResponseBody::Bytes(body) = response.body else {
            panic!("the error has a body");
        };

        assert_eq!(response.status, 500);
        assert!(String::from_utf8_lossy(&body).starts_with("Failed to prepare script"));
        assert!(termination_rx.await.unwrap().is_err());
    }
}
