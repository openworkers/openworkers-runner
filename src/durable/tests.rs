use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use openworkers_core::{
    HttpMethod, HttpRequest, ObjectIdSource, OperationsHandler, RequestBody, Script,
};

use super::id::IdKey;
use super::registry::is_running;
use super::{NamespaceConfig, ObjectSource, send};
use crate::ops::RunnerOperations;
use crate::store::Binding;

const COUNTER: &str = r#"
    export class Counter {
        constructor(state) {
            this.state = state;
            this.count = 0;
        }

        async fetch(request) {
            this.count += 1;
            await new Promise((resolve) => setTimeout(resolve, 1));
            return new Response(`${this.state.id} ${this.count}`);
        }
    }
"#;

fn namespace(id: &str) -> NamespaceConfig {
    NamespaceConfig {
        id: id.to_string(),
        // Each test has an account and a worker of its own, for the limits
        // per account and the deployments per worker
        user_id: format!("user-{id}"),
        worker_id: format!("worker-{id}"),
        class_name: "Counter".to_string(),
        key: IdKey([1; 32]),
    }
}

fn source(idle: Duration) -> ObjectSource {
    source_of(COUNTER, Vec::new(), RunnerOperations::new(), idle)
}

fn source_of(
    code: &str,
    bindings: Vec<Binding>,
    ops: RunnerOperations,
    idle: Duration,
) -> ObjectSource {
    let code = openworkers_transform::parse_worker_code(
        code.as_bytes(),
        openworkers_transform::CodeLanguage::JavaScript,
    )
    .unwrap();

    ObjectSource {
        script: Script {
            bindings: crate::store::bindings_to_infos(&bindings),
            ..Script::new(code)
        },
        version: 1,
        ops: Arc::new(ops.with_bindings(bindings)),
        limits: crate::task_executor::TaskExecutionConfig::default_limits(),
        idle,
        drain: super::DRAIN,
    }
}

fn request() -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: "http://object/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    }
}

/// Sends a request to the object `id`, and counts the loads in `loads`.
async fn get(
    namespace: &NamespaceConfig,
    id: &str,
    idle: Duration,
    loads: &Arc<AtomicUsize>,
) -> Result<String, String> {
    let loads = Arc::clone(loads);
    let load = move |_| async move {
        loads.fetch_add(1, Ordering::SeqCst);
        Ok(source(idle))
    };

    let response = tokio::time::timeout(
        Duration::from_secs(10),
        send(namespace, id.to_string(), 0, request(), load),
    )
    .await
    .expect("the object answers in time")?;

    let body = response.response.body.collect().await.unwrap().unwrap();

    Ok(String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_object_keeps_its_state_and_each_id_has_its_own() {
    let namespace = namespace("ns-state");
    let loads = Arc::new(AtomicUsize::new(0));
    let idle = Duration::from_secs(60);

    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 1");
    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 2");
    assert_eq!(get(&namespace, "b", idle, &loads).await.unwrap(), "b 1");
    assert_eq!(loads.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_requests_start_one_object() {
    let namespace = namespace("ns-burst");
    let loads = Arc::new(AtomicUsize::new(0));
    let idle = Duration::from_secs(60);

    let answers =
        futures::future::join_all((0..20).map(|_| get(&namespace, "a", idle, &loads))).await;

    // The events interleave at their await, so each one reads the count
    // after some of the others
    for answer in answers {
        answer.unwrap();
    }

    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 21");
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_object_leaves_and_a_request_starts_it_again() {
    let namespace = namespace("ns-idle");
    let loads = Arc::new(AtomicUsize::new(0));
    let idle = Duration::from_millis(100);

    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 1");
    assert!(is_running("ns-idle", "a"));

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!is_running("ns-idle", "a"));

    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 1");
    assert_eq!(loads.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_load_fails_the_request_and_the_next_one_loads_again() {
    let namespace = namespace("ns-load");
    let loads = Arc::new(AtomicUsize::new(0));

    let failing = send(&namespace, "a".to_string(), 0, request(), |_| async {
        Err("no worker".to_string())
    })
    .await;

    assert_eq!(failing.unwrap_err(), "the Durable Object stopped");
    assert_eq!(
        get(&namespace, "a", Duration::from_secs(60), &loads)
            .await
            .unwrap(),
        "a 1"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_namespace_ends_its_objects_and_refuses_requests() {
    let namespace = namespace("ns-stopped");
    let loads = Arc::new(AtomicUsize::new(0));
    let idle = Duration::from_secs(60);

    assert_eq!(get(&namespace, "a", idle, &loads).await.unwrap(), "a 1");

    // The object is busy with nothing: stopping it closes its mailbox, and
    // the task ends soon after
    super::stop_namespace("ns-stopped");

    let mut stopped = false;

    for _ in 0..50 {
        if super::stop_namespace("ns-stopped") {
            stopped = true;
            break;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(stopped, "the object still runs");
    assert!(!is_running("ns-stopped", "a"));
    assert_eq!(
        get(&namespace, "a", idle, &loads).await.unwrap_err(),
        "the Durable Object namespace was deleted"
    );
}

fn ops_with_namespace() -> RunnerOperations {
    RunnerOperations::new().with_bindings(vec![Binding::DurableObject {
        key: "COUNTER".to_string(),
        namespace: namespace("ns-ops"),
    }])
}

#[test]
fn the_binding_makes_ids_of_its_namespace() {
    let ops = ops_with_namespace();
    let key = IdKey([1; 32]);

    let named = ops
        .durable_object_id("COUNTER", ObjectIdSource::Name("a".to_string()))
        .unwrap();
    assert_eq!(named, super::id::from_name(&key, "a"));

    let unique = ops
        .durable_object_id("COUNTER", ObjectIdSource::Unique)
        .unwrap();
    assert_eq!(super::id::parse(&key, &unique), Some(unique.clone()));

    assert_eq!(
        ops.durable_object_id("COUNTER", ObjectIdSource::Text(named.to_uppercase())),
        Ok(named)
    );
    assert_eq!(
        ops.durable_object_id("COUNTER", ObjectIdSource::Text("a".to_string())),
        Err("Invalid Durable Object ID".to_string())
    );
    assert_eq!(
        ops.durable_object_id("OTHER", ObjectIdSource::Unique),
        Err("Durable Object binding 'OTHER' not configured".to_string())
    );
}

#[tokio::test]
async fn the_binding_refuses_an_id_it_did_not_make() {
    let ops = ops_with_namespace();
    let other = super::id::from_name(&IdKey([2; 32]), "a");

    let refused = ops
        .handle_binding_durable_object("COUNTER", &other, request())
        .await;
    assert_eq!(refused.unwrap_err(), "Invalid Durable Object ID");

    let unknown = ops
        .handle_binding_durable_object("OTHER", &other, request())
        .await;
    assert_eq!(
        unknown.unwrap_err(),
        "Durable Object binding 'OTHER' not configured"
    );
}

#[tokio::test]
async fn an_object_needs_the_database_to_load_its_worker() {
    let ops = ops_with_namespace();
    let id = super::id::from_name(&IdKey([1; 32]), "a");

    let answer = ops
        .handle_binding_durable_object("COUNTER", &id, request())
        .await;

    assert_eq!(answer.unwrap_err(), "the Durable Object stopped");
}

/// Calls the object `a` of its own namespace `?depth=` times in a chain, or
/// the binding `OTHER` once with `?other`.
const CALLER: &str = r#"
    export class Counter {
        constructor(state, env) {
            this.env = env;
        }

        async fetch(request) {
            const url = new URL(request.url);

            try {
                if (url.searchParams.has('other')) {
                    const stub = this.env.OTHER.getByName('x');
                    return new Response(await (await stub.fetch('http://other/')).text());
                }

                const depth = Number(url.searchParams.get('depth'));

                if (depth === 0) {
                    return new Response('end');
                }

                const stub = this.env.SELF.getByName('a');
                const answer = await stub.fetch(`http://object/?depth=${depth - 1}`);
                return new Response(`${depth} ${await answer.text()}`);
            } catch (error) {
                return new Response(error.message);
            }
        }
    }
"#;

/// Sends `path` to the object `a` of `namespace`, which runs CALLER with the
/// bindings `SELF` (its own namespace) and `OTHER` (one that never loads).
async fn call(namespace: &NamespaceConfig, path: &str) -> String {
    let bindings = vec![
        Binding::DurableObject {
            key: "SELF".to_string(),
            namespace: namespace.clone(),
        },
        Binding::DurableObject {
            key: "OTHER".to_string(),
            namespace: NamespaceConfig {
                id: format!("{}-other", namespace.id),
                ..namespace.clone()
            },
        },
    ];
    let served = namespace.clone();
    let id = super::id::from_name(&namespace.key, "a");
    let mut request = request();
    request.url = format!("http://object{path}");

    let response = tokio::time::timeout(
        Duration::from_secs(10),
        send(namespace, id, 0, request, move |depths| async move {
            let ops = RunnerOperations::new().for_object(served, "a".to_string(), depths);

            Ok(source_of(CALLER, bindings, ops, Duration::from_secs(60)))
        }),
    )
    .await
    .expect("the object answers in time")
    .unwrap();

    let body = response.response.body.collect().await.unwrap().unwrap();

    String::from_utf8(body.to_vec()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_object_makes_subrequests_past_the_count_of_a_request() {
    let namespace = namespace("ns-subrequests");

    for _ in 0..60 {
        assert_eq!(
            call(&namespace, "/?other").await,
            "the Durable Object stopped"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_object_calls_itself_in_a_chain() {
    let namespace = namespace("ns-chain");

    assert_eq!(
        call(&namespace, "/?depth=10").await,
        "10 9 8 7 6 5 4 3 2 1 end"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chain_of_calls_to_objects_has_a_limit() {
    let namespace = namespace("ns-loop");
    let answer = call(&namespace, "/?depth=1000").await;

    assert!(
        answer.ends_with("Too many nested Durable Object calls"),
        "{answer}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_runs_a_bounded_count_of_objects() {
    let resident = namespace("ns-resident");
    let neighbour = namespace("ns-resident-2");
    let loads = Arc::new(AtomicUsize::new(0));
    let idle = Duration::from_secs(60);

    let answers = futures::future::join_all((0..super::MAX_RESIDENT_PER_ACCOUNT).map(|n| {
        let namespace = &resident;
        let loads = &loads;

        async move { get(namespace, &n.to_string(), idle, loads).await }
    }))
    .await;

    for answer in answers {
        answer.unwrap();
    }

    assert_eq!(
        get(&resident, "one more", idle, &loads).await.unwrap_err(),
        "too many Durable Objects run for this account"
    );

    // A running object still takes requests, and another account its own
    assert_eq!(get(&resident, "0", idle, &loads).await.unwrap(), "0 2");
    assert_eq!(get(&neighbour, "a", idle, &loads).await.unwrap(), "a 1");

    // Its objects leave, and new ones start again
    super::stop_namespace("ns-resident");

    for _ in 0..100 {
        if super::stop_namespace("ns-resident") {
            break;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let other = NamespaceConfig {
        id: "ns-resident-3".to_string(),
        ..resident.clone()
    };
    assert_eq!(get(&other, "a", idle, &loads).await.unwrap(), "a 1");
}

/// Operations that keep the logs of the worker.
#[derive(Default)]
struct Logs(std::sync::Mutex<Vec<String>>);

impl OperationsHandler for Logs {
    fn handle_log(&self, _level: openworkers_core::LogLevel, message: String) {
        self.0.lock().unwrap().push(message);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_worker_logs_tell_why_an_object_failed() {
    let namespace = NamespaceConfig {
        class_name: "Missing".to_string(),
        ..namespace("ns-logs")
    };
    let logs = Arc::new(Logs::default());
    let ops = Arc::clone(&logs);

    let failed = send(
        &namespace,
        "a".to_string(),
        0,
        request(),
        move |_| async move {
            Ok(ObjectSource {
                ops,
                ..source(Duration::from_secs(60))
            })
        },
    )
    .await;

    assert_eq!(failed.unwrap_err(), "the Durable Object stopped");
    assert_eq!(
        *logs.0.lock().unwrap(),
        ["Durable Object Missing a: not built: TypeError: the worker exports no class 'Missing'"]
    );
}

/// Answers with the deployment it runs, as `VERSION` gives it.
const VERSIONED: &str = r#"
    export class Counter {
        constructor(state) {
            this.count = 0;
        }

        async fetch(request) {
            switch (new URL(request.url).pathname) {
                case '/slow':
                    await new Promise((resolve) => setTimeout(resolve, 300));
                    return new Response(`v${VERSION} slow`);
                case '/forever':
                    await new Promise(() => {});
                default:
                    this.count += 1;
                    return new Response(`v${VERSION} ${this.count}`);
            }
        }
    }
"#;

/// The deployments of the worker of a namespace, and when each one loaded.
#[derive(Default)]
struct Deployments {
    current: std::sync::atomic::AtomicI32,
    loads: std::sync::Mutex<Vec<(i32, std::time::Instant)>>,
}

async fn deployed(
    namespace: &NamespaceConfig,
    path: &str,
    deployments: &Arc<Deployments>,
) -> Result<String, String> {
    let deployments = Arc::clone(deployments);
    let load = move |_| async move {
        let version = deployments.current.load(Ordering::SeqCst);
        deployments
            .loads
            .lock()
            .unwrap()
            .push((version, std::time::Instant::now()));

        let code = format!("const VERSION = {version};\n{VERSIONED}");

        Ok(ObjectSource {
            version,
            drain: Duration::from_secs(1),
            ..source_of(
                &code,
                Vec::new(),
                RunnerOperations::new(),
                Duration::from_secs(60),
            )
        })
    };
    let mut request = request();
    request.url = format!("http://object{path}");

    let response = tokio::time::timeout(
        Duration::from_secs(10),
        send(namespace, "a".to_string(), 0, request, load),
    )
    .await
    .expect("the object answers in time")?;

    let body = response.response.body.collect().await.unwrap().unwrap();

    Ok(String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_deployment_starts_after_the_old_object_ends() {
    let namespace = namespace("ns-deploy");
    let deployments = Arc::new(Deployments::default());
    deployments.current.store(1, Ordering::SeqCst);

    assert_eq!(
        deployed(&namespace, "/", &deployments).await.unwrap(),
        "v1 1"
    );
    assert!(super::running_versions().contains(&("worker-ns-deploy".to_string(), 1)));

    let slow = tokio::spawn({
        let namespace = namespace.clone();
        let deployments = Arc::clone(&deployments);

        async move { deployed(&namespace, "/slow", &deployments).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    deployments.current.store(2, Ordering::SeqCst);
    super::retire("worker-ns-deploy", 1);

    // The old object ends its event; the new one starts after it, from 0
    assert_eq!(
        deployed(&namespace, "/", &deployments).await.unwrap(),
        "v2 1"
    );
    assert_eq!(slow.await.unwrap().unwrap(), "v1 slow");

    // The slow event of the old object starts after its load and takes
    // 300 ms, so the new load comes at least that much later
    let loads = deployments.loads.lock().unwrap().clone();
    assert_eq!(loads.len(), 2);
    assert!(
        loads[1].1 - loads[0].1 >= Duration::from_millis(300),
        "{loads:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_object_with_a_stuck_event_ends_after_the_drain_time() {
    let namespace = namespace("ns-deploy-stuck");
    let deployments = Arc::new(Deployments::default());
    deployments.current.store(1, Ordering::SeqCst);

    assert_eq!(
        deployed(&namespace, "/", &deployments).await.unwrap(),
        "v1 1"
    );

    let stuck = tokio::spawn({
        let namespace = namespace.clone();
        let deployments = Arc::clone(&deployments);

        async move { deployed(&namespace, "/forever", &deployments).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    deployments.current.store(2, Ordering::SeqCst);
    super::retire("worker-ns-deploy-stuck", 1);

    assert_eq!(
        deployed(&namespace, "/", &deployments).await.unwrap(),
        "v2 1"
    );
    assert_eq!(
        stuck.await.unwrap().unwrap_err(),
        "the Durable Object stopped"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn retiring_another_version_keeps_the_object() {
    let namespace = namespace("ns-deploy-other");
    let deployments = Arc::new(Deployments::default());
    deployments.current.store(3, Ordering::SeqCst);

    assert_eq!(
        deployed(&namespace, "/", &deployments).await.unwrap(),
        "v3 1"
    );
    super::retire("worker-ns-deploy-other", 2);
    assert_eq!(
        deployed(&namespace, "/", &deployments).await.unwrap(),
        "v3 2"
    );
}

const ALARMED: &str = r#"
    export class Counter {
        constructor(state) {
            this.runs = [];
        }

        async fetch() {
            return new Response(JSON.stringify(this.runs));
        }

        async alarm(info) {
            this.runs.push(info.retryCount);

            if (info.retryCount === 0) {
                throw new Error('first run fails');
            }
        }
    }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn an_alarm_runs_the_handler_of_the_object() {
    let namespace = namespace("ns-alarm");
    let load = || {
        |_| async {
            Ok(source_of(
                ALARMED,
                Vec::new(),
                RunnerOperations::new(),
                Duration::from_secs(60),
            ))
        }
    };

    assert_eq!(
        super::alarm(&namespace, "a".to_string(), 0, load()).await,
        Err(super::AlarmError::Failed("first run fails".to_string()))
    );
    assert_eq!(
        super::alarm(&namespace, "a".to_string(), 1, load()).await,
        Ok(())
    );

    let runs = send(&namespace, "a".to_string(), 0, request(), load())
        .await
        .unwrap();
    let body = runs.response.body.collect().await.unwrap().unwrap();
    assert_eq!(body.as_ref(), b"[0,1]");
}

/// Keeps the socket of its request: hibernatable on `/`, with listeners on
/// `/listen`, where the frame `break` breaks the object.
const ROOM: &str = r#"
    export class Counter {
        constructor(state) {
            this.state = state;
            this.events = 0;
        }

        async fetch(request) {
            const [client, server] = Object.values(new WebSocketPair());

            if (new URL(request.url).pathname === '/listen') {
                server.accept();
                server.addEventListener('message', () => {
                    this.state.blockConcurrencyWhile(() => {
                        throw new Error('broken');
                    });
                });
            } else {
                this.state.acceptWebSocket(server, ['room']);
                server.serializeAttachment({ joined: 1 });
            }

            server.send('hello');

            return new Response(null, { status: 101, webSocket: client });
        }

        webSocketMessage(ws, message) {
            this.events += 1;
            const count = this.state.getWebSockets('room').length;
            ws.send(`${message} ${this.events} ${count} ${ws.deserializeAttachment().joined}`);
        }
    }
"#;

/// Builds ROOM as the object `a` of `namespace`, and counts the builds.
fn room(
    namespace: NamespaceConfig,
    idle: Duration,
    loads: Arc<AtomicUsize>,
) -> super::sockets::Reload {
    Arc::new(move |depths| {
        let namespace = namespace.clone();
        let loads = Arc::clone(&loads);

        Box::pin(async move {
            loads.fetch_add(1, Ordering::SeqCst);

            let reload = room(namespace.clone(), idle, loads);
            let ops = RunnerOperations::new()
                .for_object(namespace, "a".to_string(), depths)
                .reloading_with(reload);

            Ok(source_of(ROOM, Vec::new(), ops, idle))
        })
    })
}

/// The client of a socket of an object, as the ingress holds it.
struct Client {
    to_object: tokio::sync::mpsc::UnboundedSender<openworkers_core::WebSocketIncoming>,
    from_object: tokio::sync::mpsc::UnboundedReceiver<openworkers_core::WebSocketOutgoing>,
}

impl Client {
    async fn connect(
        namespace: &NamespaceConfig,
        path: &str,
        reload: super::sockets::Reload,
    ) -> Self {
        let mut request = request();
        request.url = format!("http://object{path}");
        request
            .headers
            .insert("upgrade".to_string(), "websocket".to_string());

        let answer = send(namespace, "a".to_string(), 0, request, move |depths| {
            reload(depths)
        })
        .await
        .unwrap();
        assert_eq!(answer.response.status, 101);

        let (send_tx, from_object) = tokio::sync::mpsc::unbounded_channel();
        let (to_object, recv_rx) = tokio::sync::mpsc::unbounded_channel();
        answer
            .socket
            .expect("a 101 hands over a socket")
            .send(openworkers_core::WebSocketConnection { send_tx, recv_rx })
            .unwrap();

        Self {
            to_object,
            from_object,
        }
    }

    fn send(&self, text: &str) {
        self.to_object
            .send(openworkers_core::WebSocketIncoming::Text(text.to_string()))
            .unwrap();
    }

    async fn recv(&mut self) -> openworkers_core::WebSocketOutgoing {
        tokio::time::timeout(Duration::from_secs(5), self.from_object.recv())
            .await
            .expect("the object answers in time")
            .expect("the socket is open")
    }
}

fn text(message: openworkers_core::WebSocketOutgoing) -> String {
    match message {
        openworkers_core::WebSocketOutgoing::Text(text) => text,
        other => panic!("not a text frame: {other:?}"),
    }
}

fn close_code(message: openworkers_core::WebSocketOutgoing) -> u16 {
    match message {
        openworkers_core::WebSocketOutgoing::Close { code, .. } => code,
        other => panic!("not a close: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hibernatable_socket_starts_its_object_again() {
    let namespace = namespace("ns-socket");
    let loads = Arc::new(AtomicUsize::new(0));
    let reload = room(namespace.clone(), Duration::from_millis(100), loads.clone());

    let mut client = Client::connect(&namespace, "/", reload).await;
    assert_eq!(text(client.recv().await), "hello");

    // Past the idle time the object leaves, and its socket stays open
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!is_running("ns-socket", "a"));

    client.send("ping");
    assert_eq!(text(client.recv().await), "ping 1 1 1");
    assert_eq!(loads.load(Ordering::SeqCst), 2);

    client
        .to_object
        .send(openworkers_core::WebSocketIncoming::Closed {
            code: 1000,
            reason: String::new(),
        })
        .unwrap();

    for _ in 0..100 {
        if super::sockets::hibernatable("ns-socket", "a").is_empty() {
            return;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("a closed socket leaves its object");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_deployment_closes_the_sockets() {
    let namespace = namespace("ns-socket-deploy");
    let reload = room(namespace.clone(), Duration::from_secs(60), Arc::default());

    let mut client = Client::connect(&namespace, "/", reload).await;
    assert_eq!(text(client.recv().await), "hello");

    super::retire(&namespace.worker_id, 1);

    assert_eq!(close_code(client.recv().await), 1012);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_socket_with_listeners_closes_when_its_object_stops() {
    let namespace = namespace("ns-socket-listen");
    let reload = room(namespace.clone(), Duration::from_secs(60), Arc::default());

    let mut client = Client::connect(&namespace, "/listen", reload).await;
    assert_eq!(text(client.recv().await), "hello");

    client.send("break");

    assert_eq!(close_code(client.recv().await), 1011);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_namespace_closes_the_sockets() {
    let namespace = namespace("ns-socket-stop");
    let reload = room(namespace.clone(), Duration::from_secs(60), Arc::default());

    let mut client = Client::connect(&namespace, "/", reload).await;
    assert_eq!(text(client.recv().await), "hello");

    super::stop_namespace("ns-socket-stop");

    assert_eq!(close_code(client.recv().await), 1001);
}
