//! Worker logs for the dashboard: the logs table, and the log streams over
//! SSE and WebSocket.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use hyper::header::HeaderValue;
use hyper::{HeaderMap, Request, Response, body::Incoming};
use openworkers_core::{HyperBody, StreamBody};
use sqlx::PgPool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::log::{LogEntry, LogSink, LogStore, level_name};

/// Lines the store writes in one INSERT.
const STORE_BATCH: usize = 500;

/// Lines a new stream gets from the logs table before the live lines.
const HISTORY_LINES: i64 = 10;

/// The logs table keeps a message as varchar(255).
const MESSAGE_CHARS: i32 = 255;

/// A stream that cannot send for this long is closed.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

const SSE_KEEPALIVE: Duration = Duration::from_secs(5);
const WEBSOCKET_PING: Duration = Duration::from_secs(30);

/// The dashboard sends no large frames, so a large frame is not a log client.
const WEBSOCKET_MAX_MESSAGE: usize = 64 * 1024;

/// Writes the lines of the sink to the logs table until `stop`, then writes
/// the lines that are in the queue and returns.
pub async fn store(db: PgPool, mut store: LogStore, stop: CancellationToken) {
    let mut batch = Vec::with_capacity(STORE_BATCH);
    let mut stopping = false;

    loop {
        tokio::select! {
            received = store.lines.recv_many(&mut batch, STORE_BATCH) => {
                if received == 0 {
                    return;
                }
            }
            _ = stop.cancelled(), if !stopping => {
                stopping = true;
                store.lines.close();
                continue;
            }
        }

        if let Err(error) = insert(&db, &batch).await {
            tracing::error!(%error, lines = batch.len(), "failed to store worker logs");
        }

        batch.clear();

        let dropped = store.dropped.swap(0, Ordering::Relaxed);

        if dropped > 0 {
            tracing::warn!(
                dropped,
                "the log queue was full; worker log lines were dropped"
            );
        }
    }
}

/// Writes the lines of workers that exist; the other lines are not stored.
async fn insert(db: &PgPool, lines: &[LogEntry]) -> Result<(), sqlx::Error> {
    let mut dates = Vec::with_capacity(lines.len());
    let mut workers = Vec::with_capacity(lines.len());
    let mut messages = Vec::with_capacity(lines.len());
    let mut levels = Vec::with_capacity(lines.len());

    for line in lines {
        let Ok(worker_id) = Uuid::parse_str(&line.worker_id) else {
            continue;
        };

        dates.push(line.date);
        workers.push(worker_id);
        messages.push(line.message.as_str());
        levels.push(level_name(line.level));
    }

    if workers.is_empty() {
        return Ok(());
    }

    // The join skips a worker deleted while it ran, so its lines cannot fail the batch
    sqlx::query(
        "INSERT INTO logs (date, worker_id, message, level) \
         SELECT line.date, line.worker_id, left(line.message, $5), line.level::enum_logs_level \
         FROM UNNEST($1::timestamptz[], $2::uuid[], $3::text[], $4::text[]) \
             AS line(date, worker_id, message, level) \
         JOIN workers ON workers.id = line.worker_id",
    )
    .bind(dates)
    .bind(workers)
    .bind(messages)
    .bind(levels)
    .bind(MESSAGE_CHARS)
    .execute(db)
    .await?;

    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Sse,
    WebSocket,
}

/// The log stream a path names: `/api/v1/workers/{uuid}/logs` (SSE) or
/// `/api/v1/workers/{uuid}/ws-logs` (WebSocket).
pub fn route(path: &str) -> Option<(Uuid, Stream)> {
    let rest = path.strip_prefix("/api/v1/workers/")?;
    let (id, endpoint) = rest.split_once('/')?;

    let stream = match endpoint {
        "logs" => Stream::Sse,
        "ws-logs" => Stream::WebSocket,
        _ => return None,
    };

    Some((Uuid::parse_str(id).ok()?, stream))
}

pub struct Logs {
    db: PgPool,
    sink: LogSink,
    client: reqwest::Client,
    api_worker: String,
    stop: CancellationToken,
}

impl Logs {
    pub fn new(db: PgPool, sink: LogSink, api_worker: String, stop: CancellationToken) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(SEND_TIMEOUT)
            .build()
            .expect("the log authorization client has a static configuration");

        Self {
            db,
            sink,
            client,
            api_worker,
            stop,
        }
    }

    /// Serves a log stream to a client that the API worker lets read the
    /// worker.
    pub async fn serve(
        &self,
        mut req: Request<Incoming>,
        worker_id: Uuid,
        stream: Stream,
    ) -> Response<HyperBody> {
        if req.method() != hyper::Method::GET {
            return text(405, "GET required");
        }

        let websocket_key = match stream {
            Stream::Sse => None,
            Stream::WebSocket => {
                if !same_origin(req.headers()) {
                    return text(403, "Invalid WebSocket origin");
                }

                match websocket_accept(req.headers()) {
                    Ok(accept) => Some(accept),
                    Err(reason) => return text(400, reason),
                }
            }
        };

        let cookies = match self.authorize(req.headers(), worker_id).await {
            Ok(cookies) => cookies,
            Err((status, reason)) => return text(status, reason),
        };

        // Subscribed before the history query, so no line falls between them
        let live = self.sink.subscribe();

        let history = match history(&self.db, worker_id).await {
            Ok(history) => history,
            Err(error) => {
                tracing::error!(%error, "failed to read the log history");
                return text(503, "Log history unavailable");
            }
        };

        let follow = Follow {
            live,
            worker_id: worker_id.to_string(),
            stop: self.stop.clone(),
        };

        let mut response = match websocket_key {
            None => sse(history, follow),
            Some(accept) => {
                let upgrade = hyper::upgrade::on(&mut req);
                tokio::spawn(websocket(upgrade, history, follow));

                Response::builder()
                    .status(101)
                    .header("upgrade", "websocket")
                    .header("connection", "Upgrade")
                    .header("sec-websocket-accept", accept)
                    .body(HyperBody::Full(http_body_util::Full::new(Bytes::new())))
                    .unwrap()
            }
        };

        for cookie in cookies {
            response.headers_mut().append("set-cookie", cookie);
        }

        response
    }

    /// Asks the API worker whether the client can read the worker, with the
    /// credentials of the client. Gives the cookies that the API sets, or the
    /// status and text of the refusal.
    async fn authorize(
        &self,
        headers: &HeaderMap,
        worker_id: Uuid,
    ) -> Result<Vec<HeaderValue>, (u16, &'static str)> {
        use crate::services::fetch::{INTERNAL_ROUTE_HEADER, RUNNER_URL};

        let mut check = self
            .client
            .get(format!("{RUNNER_URL}/api/v1/workers/{worker_id}"))
            .header("x-worker-name", &self.api_worker)
            .header(INTERNAL_ROUTE_HEADER, "1")
            .header("x-request-id", Uuid::new_v4().to_string());

        for name in ["authorization", "cookie", "host", "x-forwarded-proto"] {
            if let Some(value) = headers.get(name) {
                check = check.header(name, value);
            }
        }

        let answer = match check.send().await {
            Ok(answer) => answer,
            Err(error) => {
                tracing::error!(%error, "the log authorization request failed");
                return Err((503, "Log authorization unavailable"));
            }
        };

        match answer.status().as_u16() {
            200..=299 => Ok(answer
                .headers()
                .get_all("set-cookie")
                .iter()
                .cloned()
                .collect()),
            401 => Err((401, "Unauthorized")),
            // A worker of another user is not found, so a client cannot probe ids
            403 | 404 => Err((404, "Not found")),
            status => {
                tracing::error!(status, "the log authorization request failed");
                Err((503, "Log authorization unavailable"))
            }
        }
    }
}

/// The last lines of the worker in the logs table, oldest first.
pub async fn history(db: &PgPool, worker_id: Uuid) -> Result<Vec<LogEntry>, sqlx::Error> {
    let rows: Vec<(DateTime<Utc>, String, String)> = sqlx::query_as(
        "SELECT date, level::text, message FROM logs \
         WHERE worker_id = $1 ORDER BY date DESC LIMIT $2",
    )
    .bind(worker_id)
    .bind(HISTORY_LINES)
    .fetch_all(db)
    .await?;

    let worker_id = worker_id.to_string();

    Ok(rows
        .into_iter()
        .rev()
        .map(|(date, level, message)| LogEntry {
            date,
            worker_id: worker_id.clone(),
            level: level.parse().unwrap_or(openworkers_core::LogLevel::Info),
            message,
        })
        .collect())
}

fn text(status: u16, body: &'static str) -> Response<HyperBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(HyperBody::Full(http_body_util::Full::new(
            Bytes::from_static(body.as_bytes()),
        )))
        .unwrap()
}

/// The JSON of a line in both streams: `{"date": ms, "level", "message"}`.
pub fn entry_json(entry: &LogEntry) -> String {
    serde_json::json!({
        "date": entry.date.timestamp_millis(),
        "level": level_name(entry.level),
        "message": entry.message,
    })
    .to_string()
}

pub fn sse_event(id: u64, entry: &LogEntry) -> Bytes {
    Bytes::from(format!(
        "event: log\nid: {id}\ndata: {}\n\n",
        entry_json(entry)
    ))
}

/// Whether a WebSocket request comes from a page of the same origin. A
/// client without an Origin header is not a browser page, so it passes.
pub fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get("origin") else {
        return true;
    };

    let host = headers.get("host").and_then(|host| host.to_str().ok());
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|proto| proto.to_str().ok())
        .unwrap_or("http");

    match (origin.to_str(), host) {
        (Ok(origin), Some(host)) => origin.eq_ignore_ascii_case(&format!("{scheme}://{host}")),
        _ => false,
    }
}

/// The Sec-WebSocket-Accept value of a valid version 13 upgrade request.
pub fn websocket_accept(headers: &HeaderMap) -> Result<String, &'static str> {
    use base64::Engine;

    let has_token = |name: &str, token: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|value| value.trim().eq_ignore_ascii_case(token))
    };

    if !has_token("upgrade", "websocket") || !has_token("connection", "upgrade") {
        return Err("Invalid WebSocket upgrade");
    }

    if headers
        .get("sec-websocket-version")
        .map(|version| version.as_bytes())
        != Some(b"13")
    {
        return Err("Unsupported WebSocket version");
    }

    let Some(key) = headers.get("sec-websocket-key") else {
        return Err("Missing WebSocket key");
    };

    let decoded = base64::engine::general_purpose::STANDARD.decode(key.as_bytes());

    if !decoded.is_ok_and(|key| key.len() == 16) {
        return Err("Invalid WebSocket key");
    }

    Ok(tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes()))
}

/// The live lines of one worker, until the runner stops.
struct Follow {
    live: broadcast::Receiver<LogEntry>,
    worker_id: String,
    stop: CancellationToken,
}

impl Follow {
    /// The next line, or None when the stream must close: the runner stops,
    /// or the client fell behind and must reconnect for the history.
    async fn next(&mut self) -> Option<LogEntry> {
        loop {
            let received = tokio::select! {
                _ = self.stop.cancelled() => return None,
                received = self.live.recv() => received,
            };

            match received {
                Ok(entry) if entry.worker_id == self.worker_id => return Some(entry),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }
}

async fn send_event(tx: &mpsc::Sender<Result<Bytes, String>>, event: Bytes) -> bool {
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, tx.send(Ok(event))).await,
        Ok(Ok(()))
    )
}

fn sse(history: Vec<LogEntry>, mut follow: Follow) -> Response<HyperBody> {
    let (tx, rx) = mpsc::channel(16);

    tokio::spawn(async move {
        let mut id = 0u64;

        for entry in history {
            if !send_event(&tx, sse_event(id, &entry)).await {
                return;
            }

            id += 1;
        }

        let mut keepalive = tokio::time::interval(SSE_KEEPALIVE);

        loop {
            let event = tokio::select! {
                _ = tx.closed() => return,
                _ = keepalive.tick() => Bytes::from_static(b": keepalive\n\n"),
                entry = follow.next() => match entry {
                    Some(entry) => {
                        id += 1;
                        sse_event(id - 1, &entry)
                    }
                    None => return,
                },
            };

            if !send_event(&tx, event).await {
                return;
            }
        }
    });

    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(HyperBody::Stream(StreamBody::new(rx)))
        .unwrap()
}

async fn websocket(upgrade: hyper::upgrade::OnUpgrade, history: Vec<LogEntry>, mut follow: Follow) {
    use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

    let Ok(Ok(upgraded)) = tokio::time::timeout(SEND_TIMEOUT, upgrade).await else {
        return;
    };

    let config = WebSocketConfig::default()
        .max_message_size(Some(WEBSOCKET_MAX_MESSAGE))
        .max_frame_size(Some(WEBSOCKET_MAX_MESSAGE));

    let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        Role::Server,
        Some(config),
    )
    .await;

    for entry in history {
        let message = Message::Text(entry_json(&entry).into());

        if !matches!(
            tokio::time::timeout(SEND_TIMEOUT, ws.send(message)).await,
            Ok(Ok(()))
        ) {
            return;
        }
    }

    let mut ping =
        tokio::time::interval_at(tokio::time::Instant::now() + WEBSOCKET_PING, WEBSOCKET_PING);

    loop {
        let message = tokio::select! {
            _ = ping.tick() => Message::Ping(Bytes::new()),
            entry = follow.next() => match entry {
                Some(entry) => Message::Text(entry_json(&entry).into()),
                None => break,
            },
            received = ws.next() => match received {
                Some(Ok(Message::Ping(payload))) => Message::Pong(payload),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => continue,
            },
        };

        if !matches!(
            tokio::time::timeout(SEND_TIMEOUT, ws.send(message)).await,
            Ok(Ok(()))
        ) {
            return;
        }
    }

    tokio::time::timeout(SEND_TIMEOUT, ws.close(None))
        .await
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use openworkers_core::{LogEvent, LogLevel};

    fn entry(message: &str) -> LogEntry {
        LogEntry {
            date: DateTime::from_timestamp_millis(1_760_000_000_123).unwrap(),
            worker_id: "worker".to_string(),
            level: LogLevel::Error,
            message: message.to_string(),
        }
    }

    fn line(message: &str) -> LogEvent {
        LogEvent {
            level: LogLevel::Log,
            message: message.to_string(),
        }
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();

        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }

        headers
    }

    #[test]
    fn routes_name_a_uuid_and_a_stream() {
        let id = Uuid::new_v4();

        assert_eq!(
            route(&format!("/api/v1/workers/{id}/logs")),
            Some((id, Stream::Sse))
        );
        assert_eq!(
            route(&format!("/api/v1/workers/{id}/ws-logs")),
            Some((id, Stream::WebSocket))
        );
        assert_eq!(route("/api/v1/workers/my-worker/logs"), None);
        assert_eq!(route(&format!("/api/v1/workers/{id}/logs/more")), None);
        assert_eq!(route(&format!("/api/v1/workers/{id}/upload")), None);
        assert_eq!(route(&format!("/api/v1/workers/{id}")), None);
    }

    #[test]
    fn a_line_is_json_with_a_lowercase_level() {
        let json: serde_json::Value =
            serde_json::from_str(&entry_json(&entry("a \"quote\""))).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "date": 1_760_000_000_123u64,
                "level": "error",
                "message": "a \"quote\"",
            })
        );
    }

    #[test]
    fn an_sse_event_carries_its_id_and_one_data_line() {
        assert_eq!(
            sse_event(7, &entry("two\nlines")),
            Bytes::from_static(
                b"event: log\nid: 7\ndata: {\"date\":1760000000123,\"level\":\"error\",\"message\":\"two\\nlines\"}\n\n"
            )
        );
    }

    #[test]
    fn a_websocket_page_of_another_origin_is_refused() {
        let page = |origin| {
            headers(&[
                ("host", "dash.example"),
                ("x-forwarded-proto", "https"),
                ("origin", origin),
            ])
        };

        assert!(same_origin(&page("https://dash.example")));
        assert!(!same_origin(&page("https://evil.example")));
        assert!(!same_origin(&page("http://dash.example")));
        assert!(same_origin(&headers(&[("host", "dash.example")])));
        assert!(!same_origin(&headers(&[(
            "origin",
            "https://dash.example"
        )])));
    }

    #[test]
    fn a_websocket_upgrade_needs_version_13_and_a_16_byte_key() {
        let valid = [
            ("upgrade", "websocket"),
            ("connection", "keep-alive, Upgrade"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ];

        // The example of RFC 6455, section 1.3
        assert_eq!(
            websocket_accept(&headers(&valid)).as_deref(),
            Ok("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );

        let broken = [
            ("upgrade", "h2c"),
            ("connection", "close"),
            ("sec-websocket-version", "8"),
            ("sec-websocket-key", "c2hvcnQ="),
        ];

        for (index, pair) in broken.into_iter().enumerate() {
            let mut pairs = valid;
            pairs[index] = pair;
            assert!(websocket_accept(&headers(&pairs)).is_err(), "{pair:?}");
        }

        assert_eq!(
            websocket_accept(&headers(&valid[..3])),
            Err("Missing WebSocket key")
        );
    }

    #[tokio::test]
    async fn a_stream_skips_other_workers_and_closes_when_it_falls_behind() {
        let (sink, _store) = LogSink::new();
        let mut follow = Follow {
            live: sink.subscribe(),
            worker_id: "worker".to_string(),
            stop: CancellationToken::new(),
        };

        sink.send("other", line("no"));
        sink.send("worker", line("yes"));
        assert_eq!(follow.next().await.unwrap().message, "yes");

        for _ in 0..2000 {
            sink.send("other", line("flood"));
        }

        assert!(follow.next().await.is_none());
    }

    #[tokio::test]
    async fn a_stream_closes_when_the_runner_stops() {
        let (sink, _store) = LogSink::new();
        let stop = CancellationToken::new();
        let mut follow = Follow {
            live: sink.subscribe(),
            worker_id: "worker".to_string(),
            stop: stop.clone(),
        };

        stop.cancel();

        assert!(follow.next().await.is_none());
    }
}
