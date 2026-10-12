//! The WebSockets of a worker. The runner holds the connection; two tasks move
//! its frames to and from the channels the runtime reads, for an outgoing
//! connection and for an upgraded request alike.

use base64::Engine;
use futures::{SinkExt, StreamExt};
use hyper::header::{CONNECTION, UPGRADE};
use hyper::{HeaderMap, Method, Response, Version};
use openworkers_core::{
    HttpResponse, HyperBody, WebSocketConnection, WebSocketIncoming, WebSocketOutgoing,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::oneshot;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite;
use tungstenite::protocol::{Role, WebSocketConfig};

/// The largest message, and frame, a client can send. A larger one closes the
/// socket with 1009.
pub const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// What a request asks of the WebSocket protocol.
#[derive(Debug, PartialEq, Eq)]
pub enum Upgrade {
    /// A plain request
    None,
    /// A valid upgrade, answered with this `Sec-WebSocket-Accept`
    Accept(String),
    /// An upgrade the runner refuses with this status
    Refused(u16, &'static str),
}

/// Whether a header lists `token`, as `Connection: keep-alive, Upgrade` does.
fn lists(headers: &HeaderMap, name: hyper::header::HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|item| item.trim().eq_ignore_ascii_case(token))
}

/// Reads the upgrade of RFC 6455 section 4.2.1. HTTP/2 has no Upgrade
/// header, so only an HTTP/1.1 request can ask.
pub fn upgrade(version: Version, method: &Method, headers: &HeaderMap) -> Upgrade {
    if version != Version::HTTP_11 || !lists(headers, UPGRADE, "websocket") {
        return Upgrade::None;
    }

    if method != Method::GET {
        return Upgrade::Refused(400, "A WebSocket upgrade must be a GET");
    }

    if !lists(headers, CONNECTION, "upgrade") {
        return Upgrade::Refused(400, "A WebSocket upgrade must have Connection: Upgrade");
    }

    if headers
        .get("sec-websocket-version")
        .is_none_or(|version| version != "13")
    {
        return Upgrade::Refused(426, "The WebSocket version must be 13");
    }

    let Some(key) = headers.get("sec-websocket-key") else {
        return Upgrade::Refused(400, "A WebSocket upgrade must have a Sec-WebSocket-Key");
    };

    let nonce = base64::engine::general_purpose::STANDARD.decode(key.as_bytes());

    if nonce.map_or(true, |nonce| nonce.len() != 16) {
        return Upgrade::Refused(400, "The Sec-WebSocket-Key is not a 16-byte nonce");
    }

    Upgrade::Accept(tungstenite::handshake::derive_accept_key(key.as_bytes()))
}

/// The 101 that switches the connection of an upgrade, with the headers the
/// worker set. Once hyper writes it, the socket goes to the worker on
/// `socket_tx`. When the upgrade fails, dropping `socket_tx` closes the
/// server end of the worker.
pub fn switch(
    response: HttpResponse,
    accept: String,
    on_upgrade: hyper::upgrade::OnUpgrade,
    socket_tx: oneshot::Sender<WebSocketConnection>,
) -> Response<HyperBody> {
    tokio::spawn(async move {
        let upgraded = match on_upgrade.await {
            Ok(upgraded) => upgraded,
            Err(error) => {
                tracing::debug!(%error, "websocket upgrade failed");
                return;
            }
        };

        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let stream = WebSocketStream::from_raw_socket(
            hyper_util::rt::TokioIo::new(upgraded),
            Role::Server,
            Some(config),
        )
        .await;

        // The worker let the socket go before the client came
        if let Err(socket) = socket_tx.send(bridge(stream)) {
            let _ = socket.send_tx.send(WebSocketOutgoing::Close {
                code: 1000,
                reason: String::new(),
            });
        }
    });

    let mut builder = Response::builder().status(101);

    for (name, value) in &response.headers {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "connection"
                | "upgrade"
                | "sec-websocket-accept"
                | "content-length"
                | "transfer-encoding"
        ) {
            continue;
        }

        builder = builder.header(name, value);
    }

    builder
        .header(UPGRADE, "websocket")
        .header(CONNECTION, "Upgrade")
        .header("sec-websocket-accept", accept)
        .body(HyperBody::Full(http_body_util::Full::new(
            bytes::Bytes::new(),
        )))
        .expect("the worker headers passed the response checks")
}

/// Moves the frames of `stream` through two tasks, and gives the channels to
/// them. Dropping the sender closes the socket.
pub fn bridge<S>(stream: WebSocketStream<S>) -> WebSocketConnection
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut ws_write, mut ws_read) = stream.split();

    let (send_tx, mut send_rx) = tokio::sync::mpsc::unbounded_channel::<WebSocketOutgoing>();
    let (recv_tx, recv_rx) = tokio::sync::mpsc::unbounded_channel::<WebSocketIncoming>();

    // JS -> WS
    tokio::spawn(async move {
        while let Some(msg) = send_rx.recv().await {
            let ws_msg = match msg {
                WebSocketOutgoing::Text(s) => tungstenite::Message::Text(s.into()),
                WebSocketOutgoing::Binary(b) => tungstenite::Message::Binary(bytes::Bytes::from(b)),
                WebSocketOutgoing::Close { code, reason } => {
                    let frame = tungstenite::protocol::CloseFrame {
                        code: tungstenite::protocol::frame::coding::CloseCode::from(code),
                        reason: reason.into(),
                    };
                    tungstenite::Message::Close(Some(frame))
                }
            };

            if ws_write.send(ws_msg).await.is_err() {
                break;
            }
        }

        let _ = ws_write.close().await;
    });

    // WS -> JS
    let close_tx = send_tx.clone();

    tokio::spawn(async move {
        while let Some(result) = ws_read.next().await {
            let incoming = match result {
                Ok(tungstenite::Message::Text(s)) => WebSocketIncoming::Text(s.to_string()),
                Ok(tungstenite::Message::Binary(b)) => WebSocketIncoming::Binary(b.to_vec()),
                Ok(tungstenite::Message::Close(frame)) => {
                    let (code, reason) = frame
                        .map(|f| (u16::from(f.code), f.reason.to_string()))
                        .unwrap_or((1000, String::new()));
                    let _ = recv_tx.send(WebSocketIncoming::Closed { code, reason });
                    break;
                }
                Ok(tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_)) => {
                    continue; // tungstenite handles pong automatically
                }
                Ok(tungstenite::Message::Frame(_)) => continue,
                // tungstenite stops reading without a close frame, so both
                // ends learn of the limit here
                Err(tungstenite::Error::Capacity(_)) => {
                    let reason = "Message too big".to_string();
                    let _ = close_tx.send(WebSocketOutgoing::Close {
                        code: 1009,
                        reason: reason.clone(),
                    });
                    let _ = recv_tx.send(WebSocketIncoming::Closed { code: 1009, reason });
                    break;
                }
                Err(e) => {
                    let _ = recv_tx.send(WebSocketIncoming::Error(format!("WS error: {}", e)));
                    break;
                }
            };

            if recv_tx.send(incoming).is_err() {
                break; // Receiver dropped
            }
        }
    });

    WebSocketConnection { send_tx, recv_rx }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", "websocket".parse().unwrap());
        headers.insert("connection", "keep-alive, Upgrade".parse().unwrap());
        headers.insert("sec-websocket-version", "13".parse().unwrap());
        headers.insert(
            "sec-websocket-key",
            "dGhlIHNhbXBsZSBub25jZQ==".parse().unwrap(),
        );
        headers
    }

    /// The sample handshake of RFC 6455 section 1.3.
    #[test]
    fn the_rfc_sample_gets_its_accept_key() {
        assert_eq!(
            upgrade(Version::HTTP_11, &Method::GET, &handshake()),
            Upgrade::Accept("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".into())
        );
    }

    #[test]
    fn a_plain_request_is_not_an_upgrade() {
        let mut headers = handshake();
        headers.remove("upgrade");

        assert_eq!(
            upgrade(Version::HTTP_11, &Method::GET, &headers),
            Upgrade::None
        );
        assert_eq!(
            upgrade(Version::HTTP_2, &Method::GET, &handshake()),
            Upgrade::None
        );
    }

    #[test]
    fn a_bad_handshake_is_refused() {
        let refused =
            |headers: &HeaderMap, method: &Method| match upgrade(Version::HTTP_11, method, headers)
            {
                Upgrade::Refused(status, _) => status,
                other => panic!("not refused: {other:?}"),
            };

        assert_eq!(refused(&handshake(), &Method::POST), 400);

        let mut headers = handshake();
        headers.insert("sec-websocket-version", "8".parse().unwrap());
        assert_eq!(refused(&headers, &Method::GET), 426);

        let mut headers = handshake();
        headers.insert("sec-websocket-key", "c2hvcnQ=".parse().unwrap());
        assert_eq!(refused(&headers, &Method::GET), 400);

        let mut headers = handshake();
        headers.remove("connection");
        assert_eq!(refused(&headers, &Method::GET), 400);
    }
}
