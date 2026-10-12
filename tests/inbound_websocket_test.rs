//! A WebSocket client talks to a worker through a real hyper connection: the
//! runner reads the handshake, answers the 101 and bridges the socket to the
//! server end of a WebSocketPair.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{CloseCode, Data, OpCode};

use openworkers_core::{Event, FetchInit, HttpRequest, HyperBody, Script};
use openworkers_runner::RunnerOperations;
use openworkers_runner::websocket::{self, Upgrade};
use openworkers_runner::worker_pool::WORKER_POOL;
use openworkers_runtime_v8::Worker;
use serial_test::serial;

/// Short, so a test can wait it out. The runner reads WS_MAX_IDLE_MS.
const MAX_IDLE_MS: u64 = 500;

const ECHO: &str = r#"
    addEventListener('fetch', (event) => {
        const [client, server] = Object.values(new WebSocketPair());

        server.accept();
        server.addEventListener('message', (e) => server.send(e.data));

        event.respondWith(new Response(null, {
            status: 101,
            webSocket: client,
            headers: { 'Sec-WebSocket-Protocol': 'echo' },
        }));
    });
"#;

/// Serves each connection with the upgrade path of bin/main.rs, cut down to
/// what a socket needs.
async fn serve(code: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let service = service_fn(move |req| handle(code, req));

            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });

    format!("ws://{address}/socket")
}

async fn handle(
    code: &'static str,
    mut req: Request<hyper::body::Incoming>,
) -> Result<Response<HyperBody>, std::convert::Infallible> {
    let Upgrade::Accept(accept) = websocket::upgrade(req.version(), req.method(), req.headers())
    else {
        panic!("the client asks for an upgrade");
    };

    let on_upgrade = hyper::upgrade::on(&mut req);
    let request = HttpRequest::from_hyper_parts(
        req.method(),
        req.uri(),
        req.headers(),
        bytes::Bytes::new(),
        "http",
    );

    let (res_tx, res_rx) = oneshot::channel();
    let (init, socket_tx) = FetchInit::upgradable(request, res_tx);

    WORKER_POOL.spawn(move || async move {
        let ops = Arc::new(RunnerOperations::new());
        let mut worker = Worker::new_with_ops(Script::new(code), None, ops)
            .await
            .unwrap();

        let _ = worker.exec(Event::Fetch(Some(init))).await;
    });

    let response = res_rx.await.expect("the worker answers");
    assert_eq!(response.status, 101);

    Ok(websocket::switch(
        response,
        accept,
        on_upgrade,
        socket_tx,
        Duration::from_millis(MAX_IDLE_MS),
    ))
}

async fn connect(url: &str) -> tokio_tungstenite::WebSocketStream<TcpStream> {
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", "echo".parse().unwrap());

    let address = url.trim_start_matches("ws://").split('/').next().unwrap();
    let stream = TcpStream::connect(address).await.unwrap();

    let (socket, response) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("the handshake succeeds");

    assert_eq!(response.headers()["sec-websocket-protocol"], "echo");

    socket
}

async fn next(socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>) -> Message {
    tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the worker answers")
        .expect("the socket is open")
        .expect("the frame is valid")
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_client_talks_to_the_worker() {
    let url = serve(ECHO).await;
    let mut socket = connect(&url).await;

    socket.send(Message::text("ping")).await.unwrap();
    assert_eq!(next(&mut socket).await, Message::text("ping"));

    let bytes = bytes::Bytes::from_static(&[0, 1, 2, 255]);
    socket.send(Message::Binary(bytes.clone())).await.unwrap();
    assert_eq!(next(&mut socket).await, Message::Binary(bytes));

    socket.close(None).await.unwrap();

    match next(&mut socket).await {
        Message::Close(_) => {}
        other => panic!("not a close: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_message_over_the_limit_closes_the_socket() {
    let url = serve(ECHO).await;
    let mut socket = connect(&url).await;

    // Two frames under the limit, so the runner reads both before it refuses
    // the message, and the close frame is not lost to a reset connection
    let half = bytes::Bytes::from(vec![0u8; websocket::MAX_MESSAGE_BYTES / 2 + 1]);
    let first = Frame::message(half.clone(), OpCode::Data(Data::Binary), false);
    let last = Frame::message(half, OpCode::Data(Data::Continue), true);

    socket.send(Message::Frame(first)).await.unwrap();
    socket.send(Message::Frame(last)).await.unwrap();

    match next(&mut socket).await {
        Message::Close(Some(frame)) => assert_eq!(frame.code, CloseCode::Size),
        other => panic!("not a close: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_idle_socket_closes() {
    let url = serve(ECHO).await;
    let mut socket = connect(&url).await;

    match next(&mut socket).await {
        Message::Close(Some(frame)) => {
            assert_eq!(frame.code, CloseCode::Away);
            assert_eq!(frame.reason, "Idle for too long");
        }
        other => panic!("not a close: {other:?}"),
    }
}

/// Answers only when asked, so the client alone keeps the socket busy.
const COUNTER: &str = r#"
    addEventListener('fetch', (event) => {
        const [client, server] = Object.values(new WebSocketPair());
        let count = 0;

        server.accept();
        server.addEventListener('message', (e) => {
            if (e.data === 'count') server.send(String(count));
            else count += 1;
        });

        event.respondWith(new Response(null, {
            status: 101,
            webSocket: client,
            headers: { 'Sec-WebSocket-Protocol': 'echo' },
        }));
    });
"#;

/// Talks on its own, so the worker alone keeps the socket busy.
const TICKER: &str = r#"
    addEventListener('fetch', (event) => {
        const [client, server] = Object.values(new WebSocketPair());

        server.accept();
        const timer = setInterval(() => server.send('tick'), 200);
        server.addEventListener('close', () => clearInterval(timer));

        event.respondWith(new Response(null, {
            status: 101,
            webSocket: client,
            headers: { 'Sec-WebSocket-Protocol': 'echo' },
        }));
    });
"#;

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn frames_from_the_client_keep_the_socket_open() {
    let url = serve(COUNTER).await;
    let mut socket = connect(&url).await;

    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(MAX_IDLE_MS / 2)).await;
        socket.send(Message::text("one")).await.unwrap();
    }

    socket.send(Message::text("count")).await.unwrap();
    assert_eq!(next(&mut socket).await, Message::text("6"));
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn frames_from_the_worker_keep_the_socket_open() {
    let url = serve(TICKER).await;
    let mut socket = connect(&url).await;

    // 1.6 s of ticks, three times the idle limit
    for _ in 0..8 {
        assert_eq!(next(&mut socket).await, Message::text("tick"));
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_drain_closes_the_socket() {
    let url = serve(ECHO).await;
    let mut socket = connect(&url).await;

    socket.send(Message::text("before")).await.unwrap();
    assert_eq!(next(&mut socket).await, Message::text("before"));

    openworkers_runner::worker_pool::set_draining(true);
    let frame = next(&mut socket).await;
    openworkers_runner::worker_pool::set_draining(false);

    match frame {
        Message::Close(Some(frame)) => {
            assert_eq!(frame.code, CloseCode::Away);
            assert_eq!(frame.reason, "The runner is going away");
        }
        other => panic!("not a close: {other:?}"),
    }
}
