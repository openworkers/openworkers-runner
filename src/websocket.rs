//! The WebSockets of a worker. The runner holds the connection; two tasks move
//! its frames to and from the channels the runtime reads.

use futures::{SinkExt, StreamExt};
use openworkers_core::{WebSocketConnection, WebSocketIncoming, WebSocketOutgoing};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite;

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
