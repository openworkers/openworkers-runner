//! The abort token scopes a request's ops without cutting its waitUntil work.
//!
//! Cancelling it settles whatever op the guest still awaits. Leaving it alone
//! must let background work after the response run to completion: a token
//! cancelled on "the body was dropped" once killed every waitUntil(fetch()).

use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::HttpResponse;
use openworkers_core::OpFuture;
use openworkers_core::OperationsHandler;
use openworkers_core::RequestBody;
use openworkers_core::ResponseBody;
use openworkers_core::RuntimeLimits;
use openworkers_core::Script;
use openworkers_runtime_v8::PinnedExecuteRequest;
use openworkers_runtime_v8::PinnedPoolConfig;
use openworkers_runtime_v8::execute_pinned;
use openworkers_runtime_v8::init_pinned_pool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

static INIT: Once = Once::new();

fn init_pool() {
    INIT.call_once(|| {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });
    });
}

/// A fetch that takes a moment, and counts the times it got to the end.
struct SlowFetch {
    completed: Arc<AtomicUsize>,
}

impl OperationsHandler for SlowFetch {
    fn handle_fetch(&self, _request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        let completed = self.completed.clone();

        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            completed.fetch_add(1, Ordering::SeqCst);

            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: ResponseBody::None,
            })
        })
    }
}

const SCRIPT: &str = r#"
    addEventListener('fetch', (event) => {
        event.respondWith(new Response('ok'));
        event.waitUntil(fetch('https://example.com/beacon').then(() => {}, () => {}));
    });
"#;

async fn run(abort: CancellationToken, worker_id: &str) -> (u16, usize) {
    let completed = Arc::new(AtomicUsize::new(0));
    let ops: Arc<dyn OperationsHandler> = Arc::new(SlowFetch {
        completed: completed.clone(),
    });

    let request = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };
    let (task, rx) = Event::fetch(request);

    let local = tokio::task::LocalSet::new();
    let status = local
        .run_until(async {
            let finished = tokio::time::timeout(
                Duration::from_secs(5),
                execute_pinned(PinnedExecuteRequest {
                    owner_id: "abort-scope".to_string(),
                    worker_id: worker_id.to_string(),
                    version: 1,
                    script: Script::new(SCRIPT),
                    ops,
                    task,
                    on_warm_hit: None,
                    env_updated_at: None,
                    abort: Some(abort),
                }),
            )
            .await;

            finished
                .expect("execute_pinned must return, whatever happens to the scope")
                .unwrap();

            rx.await
                .expect("the response was sent before any waitUntil ran")
                .status
        })
        .await;

    (status, completed.load(Ordering::SeqCst))
}

#[tokio::test]
async fn an_uncancelled_scope_lets_waituntil_finish() {
    init_pool();

    let (status, completed) = run(CancellationToken::new(), "waituntil-runs").await;

    assert_eq!(status, 200);
    assert_eq!(
        completed, 1,
        "waitUntil(fetch()) after the response must run to the end"
    );
}

#[tokio::test]
async fn a_cancelled_scope_settles_waituntil_instead_of_hanging() {
    init_pool();

    let abort = CancellationToken::new();

    tokio::spawn({
        let abort = abort.clone();

        async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            abort.cancel();
        }
    });

    let (status, completed) = run(abort, "waituntil-cut").await;

    assert_eq!(
        status, 200,
        "the response left before the scope was cancelled"
    );
    assert_eq!(
        completed, 0,
        "a cancelled scope must drop the op, not let it finish"
    );
}
