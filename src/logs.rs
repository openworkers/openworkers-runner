use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use hyper::{Request, Response, body::Incoming};
use openworkers_core::{HyperBody, StreamBody};
use sqlx::{FromRow, PgPool};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, serde::Serialize, FromRow)]
pub struct Entry {
    pub date: DateTime<Utc>,
    pub worker_id: Uuid,
    pub level: String,
    pub message: String,
}
impl Entry {
    fn json(&self) -> String {
        serde_json::json!({"date": self.date.timestamp_millis(), "level": self.level, "message": self.message}).to_string()
    }
}

pub struct Logs {
    db: PgPool,
    live: broadcast::Sender<Entry>,
    client: reqwest::Client,
    stop: CancellationToken,
}

pub fn full(status: u16, text: impl Into<Bytes>) -> Response<HyperBody> {
    Response::builder()
        .status(status)
        .body(HyperBody::Full(http_body_util::Full::new(text.into())))
        .unwrap()
}

impl Logs {
    pub async fn start(
        db: PgPool,
        stop: CancellationToken,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let nc = crate::nats::nats_connect().await;
        let mut subscription = nc.subscribe("*.console.*").await?;
        nc.flush().await?;
        let (live, _) = broadcast::channel(1024);
        let logs = Arc::new(Self {
            db,
            live,
            stop,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
        });
        let copy = logs.clone();
        tokio::spawn(async move {
            loop {
                let msg = tokio::select! {
                    _ = copy.stop.cancelled() => break,
                    msg = subscription.next() => match msg { Some(msg) => msg, None => break }
                };
                let parts: Vec<_> = msg.subject.split('.').collect();
                if parts.len() != 3 {
                    continue;
                }
                let Ok(worker_id) = Uuid::parse_str(parts[0]) else {
                    continue;
                };
                let Ok(message) = String::from_utf8(msg.payload.to_vec()) else {
                    continue;
                };
                let level = match parts[2] {
                    "error" | "warn" | "info" | "log" | "debug" | "trace" => parts[2],
                    _ => "info",
                };
                let entry = Entry {
                    date: Utc::now(),
                    worker_id,
                    level: level.into(),
                    message,
                };
                let _ = copy.live.send(entry.clone());
                let message: String = entry.message.chars().take(255).collect();
                if let Err(error) = sqlx::query("INSERT INTO logs (date, worker_id, message, level) VALUES ($1,$2,$3,$4::enum_logs_level)")
                    .bind(entry.date).bind(worker_id).bind(message).bind(level).execute(&copy.db).await {
                    tracing::error!(%error, "log persistence failed");
                }
            }
        });
        Ok(logs)
    }

    pub async fn serve(
        self: &Arc<Self>,
        mut req: Request<Incoming>,
        id: Uuid,
        websocket: bool,
        api_worker: &str,
    ) -> Response<HyperBody> {
        if websocket && let Some(origin) = req.headers().get("origin") {
            let expected = req
                .headers()
                .get("host")
                .and_then(|h| h.to_str().ok())
                .map(|host| {
                    format!(
                        "{}://{host}",
                        req.headers()
                            .get("x-forwarded-proto")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("http")
                    )
                });
            if origin.to_str().ok() != expected.as_deref() {
                return full(403, "Invalid WebSocket origin");
            }
        }
        // The API checks the session and access to this worker.
        let mut check = self
            .client
            .get(format!("http://127.0.0.1:8080/api/v1/workers/{id}"))
            .header("x-worker-name", api_worker)
            .header("x-openworkers-internal", "1")
            .header("x-request-id", Uuid::new_v4().to_string());
        for name in ["authorization", "cookie", "host", "x-forwarded-proto"] {
            if let Some(value) = req.headers().get(name) {
                check = check.header(name, value);
            }
        }
        let auth = match check.send().await {
            Ok(response) => response,
            Err(_) => return full(503, "Log authorization unavailable"),
        };
        if !auth.status().is_success() {
            return full(
                match auth.status().as_u16() {
                    401 => 401,
                    403 | 404 => 404,
                    _ => 503,
                },
                "Logs unavailable",
            );
        }
        let cookies: Vec<_> = auth
            .headers()
            .get_all("set-cookie")
            .iter()
            .cloned()
            .collect();
        let mut live = self.live.subscribe();
        let history = match sqlx::query_as::<_, Entry>("SELECT date, worker_id, level::text AS level, message FROM logs WHERE worker_id=$1 ORDER BY date DESC LIMIT 10")
            .bind(id).fetch_all(&self.db).await {
            Ok(history) => history,
            Err(_) => return full(503, "Log history unavailable"),
        };
        let mut response = if websocket {
            use tokio_tungstenite::tungstenite::{
                Message, handshake::derive_accept_key, protocol::Role,
            };
            let headers = req.headers();
            if headers
                .get("sec-websocket-version")
                .and_then(|h| h.to_str().ok())
                != Some("13")
                || !headers
                    .get("upgrade")
                    .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
                || !headers
                    .get("connection")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| {
                        v.split(',')
                            .any(|s| s.trim().eq_ignore_ascii_case("upgrade"))
                    })
            {
                return full(400, "Invalid WebSocket upgrade");
            }
            let Some(key) = headers.get("sec-websocket-key") else {
                return full(400, "Missing WebSocket key");
            };
            use base64::Engine;
            if !base64::engine::general_purpose::STANDARD
                .decode(key.as_bytes())
                .is_ok_and(|v| v.len() == 16)
            {
                return full(400, "Invalid WebSocket key");
            }
            let accept = derive_accept_key(key.as_bytes());
            let upgrade = hyper::upgrade::on(&mut req);
            let stop = self.stop.clone();
            tokio::spawn(async move {
                let Ok(Ok(upgraded)) =
                    tokio::time::timeout(std::time::Duration::from_secs(10), upgrade).await
                else {
                    return;
                };
                let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                    hyper_util::rt::TokioIo::new(upgraded),
                    Role::Server,
                    None,
                )
                .await;
                for entry in history.into_iter().rev() {
                    if !matches!(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            ws.send(Message::Text(entry.json().into()))
                        )
                        .await,
                        Ok(Ok(()))
                    ) {
                        return;
                    }
                }
                let mut ping = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    let out = tokio::select! {
                        _ = stop.cancelled() => break,
                        entry = live.recv() => match entry {
                            Ok(entry) if entry.worker_id == id => Message::Text(entry.json().into()),
                            Ok(_) => continue,
                            Err(_) => break, // Slow consumers reconnect for history.
                        },
                        msg = ws.next() => match msg {
                            Some(Ok(Message::Ping(bytes))) => Message::Pong(bytes),
                            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                            _ => continue,
                        },
                        _ = ping.tick() => Message::Ping(Bytes::new()),
                    };
                    if !matches!(
                        tokio::time::timeout(std::time::Duration::from_secs(10), ws.send(out))
                            .await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(2), ws.close(None)).await;
            });
            Response::builder()
                .status(101)
                .header("upgrade", "websocket")
                .header("connection", "Upgrade")
                .header("sec-websocket-accept", accept)
                .body(HyperBody::Full(http_body_util::Full::new(Bytes::new())))
                .unwrap()
        } else {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            let stop = self.stop.clone();
            tokio::spawn(async move {
                let mut counter = 0u64;
                for entry in history.into_iter().rev() {
                    if tx
                        .send(Ok(Bytes::from(format!(
                            "event: log\nid: {counter}\ndata: {}\n\n",
                            entry.json()
                        ))))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    counter += 1;
                }
                let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(5));
                loop {
                    let bytes = tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = tx.closed() => break,
                        _ = keepalive.tick() => Bytes::from_static(b": keepalive\n\n"),
                        entry = live.recv() => match entry {
                            Ok(entry) if entry.worker_id == id => {
                                let text = format!("event: log\nid: {counter}\ndata: {}\n\n", entry.json());
                                counter += 1;
                                Bytes::from(text)
                            },
                            Ok(_) => continue,
                            Err(_) => break,
                        }
                    };
                    if !matches!(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            tx.send(Ok(bytes))
                        )
                        .await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
            });
            Response::builder()
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .body(HyperBody::Stream(StreamBody::new(rx)))
                .unwrap()
        };
        for cookie in cookies {
            response.headers_mut().append("set-cookie", cookie);
        }
        response
    }
}
