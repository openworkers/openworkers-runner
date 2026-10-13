//! The WebSockets of Durable Objects. The runner holds them, so a
//! hibernatable socket outlives the instance of its object, and a frame of
//! the client starts the object again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use once_cell::sync::Lazy;
use openworkers_core::{
    ObjectSocket, WebSocketConnection, WebSocketId, WebSocketIncoming, WebSocketOutgoing,
    WebSocketUpgrade,
};
use openworkers_runtime_v8::SocketEvent;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use super::{CallDepths, NamespaceConfig, ObjectSource};
use crate::websocket::MAX_QUEUED_BYTES;

/// The open sockets of one object, as on Cloudflare
pub const MAX_SOCKETS_PER_OBJECT: usize = 32_768;

/// A socket whose client is not there this long after the accept closes
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);

/// Builds an object again, for a frame that comes when it does not run.
pub type Reload = Arc<
    dyn Fn(Arc<CallDepths>) -> futures::future::BoxFuture<'static, Result<ObjectSource, String>>
        + Send
        + Sync,
>;

/// What starts an object again for a frame of its client.
#[derive(Clone)]
pub struct Wake {
    pub namespace: NamespaceConfig,
    pub reload: Reload,
}

struct Socket {
    /// Frames and the close for the client, in order. The task of the
    /// socket sends them once the client is there.
    out: mpsc::UnboundedSender<WebSocketOutgoing>,
    /// Taken by the response that hands the socket over
    attach: Option<oneshot::Sender<WebSocketUpgrade>>,
    tags: Vec<String>,
    attachment: Option<Vec<u8>>,
    hibernatable: bool,
}

/// An object: its namespace and its id
type ObjectRef = (String, String);

static SOCKETS: Lazy<Mutex<HashMap<ObjectRef, HashMap<WebSocketId, Socket>>>> =
    Lazy::new(Default::default);

fn key(namespace_id: &str, object_id: &str) -> ObjectRef {
    (namespace_id.to_string(), object_id.to_string())
}

/// The object `object_id` takes `socket`, the server end of a pair it made.
pub fn accept(
    wake: Wake,
    object_id: &str,
    socket: WebSocketId,
    tags: Vec<String>,
    hibernatable: bool,
) -> Result<(), String> {
    let key = key(&wake.namespace.id, object_id);
    let (out, out_rx) = mpsc::unbounded_channel();
    let (attach, attached) = oneshot::channel();

    {
        let mut sockets = SOCKETS.lock().unwrap();
        let object = sockets.entry(key.clone()).or_default();

        if object.len() >= MAX_SOCKETS_PER_OBJECT {
            return Err(format!(
                "a Durable Object has at most {MAX_SOCKETS_PER_OBJECT} WebSockets"
            ));
        }

        object.insert(
            socket,
            Socket {
                out,
                attach: Some(attach),
                tags,
                attachment: None,
                hibernatable,
            },
        );
    }

    tokio::spawn(run(key, socket, wake, out_rx, attached));

    Ok(())
}

/// The response that carries `socket` is out, and the socket of the client
/// comes on `upgrade`. Answers false for a socket the object did not accept.
pub fn attach(
    namespace_id: &str,
    object_id: &str,
    socket: WebSocketId,
    upgrade: WebSocketUpgrade,
) -> bool {
    let attach = SOCKETS
        .lock()
        .unwrap()
        .get_mut(&key(namespace_id, object_id))
        .and_then(|object| object.get_mut(&socket))
        .and_then(|socket| socket.attach.take());

    attach.is_some_and(|attach| attach.send(upgrade).is_ok())
}

/// A frame for the client of `socket`, or its close. A closed socket leaves
/// the object at once; its task still sends the close.
pub fn send(namespace_id: &str, object_id: &str, socket: WebSocketId, message: WebSocketOutgoing) {
    let closes = matches!(message, WebSocketOutgoing::Close { .. });
    let mut sockets = SOCKETS.lock().unwrap();
    let Some(object) = sockets.get_mut(&key(namespace_id, object_id)) else {
        return;
    };

    if let Some(open) = object.get(&socket) {
        let _ = open.out.send(message);
    }

    if closes {
        object.remove(&socket);
    }
}

pub fn set_attachment(
    namespace_id: &str,
    object_id: &str,
    socket: WebSocketId,
    value: Option<Vec<u8>>,
) {
    if let Some(open) = SOCKETS
        .lock()
        .unwrap()
        .get_mut(&key(namespace_id, object_id))
        .and_then(|object| object.get_mut(&socket))
    {
        open.attachment = value;
    }
}

/// The hibernatable sockets of an object, for a new instance of it.
pub fn hibernatable(namespace_id: &str, object_id: &str) -> Vec<ObjectSocket> {
    let sockets = SOCKETS.lock().unwrap();
    let Some(object) = sockets.get(&key(namespace_id, object_id)) else {
        return Vec::new();
    };

    object
        .iter()
        .filter(|(_, socket)| socket.hibernatable)
        .map(|(id, socket)| ObjectSocket {
            id: *id,
            tags: socket.tags.clone(),
            attachment: socket.attachment.clone(),
        })
        .collect()
}

/// Closes the sockets that `which` selects.
fn close_where(which: impl Fn(&ObjectRef, &Socket) -> bool, code: u16, reason: &str) {
    let mut sockets = SOCKETS.lock().unwrap();

    for (key, object) in sockets.iter_mut() {
        object.retain(|_, socket| {
            if !which(key, socket) {
                return true;
            }

            let _ = socket.out.send(WebSocketOutgoing::Close {
                code,
                reason: reason.to_string(),
            });

            false
        });
    }

    sockets.retain(|_, object| !object.is_empty());
}

/// Closes the sockets of an object, as for a new deployment of its worker.
pub fn close_object(namespace_id: &str, object_id: &str, code: u16, reason: &str) {
    close_where(
        |key, _| key.0 == namespace_id && key.1 == object_id,
        code,
        reason,
    );
}

/// Closes the sockets of the objects of a deleted namespace.
pub fn close_namespace(namespace_id: &str) {
    close_where(
        |key, _| key.0 == namespace_id,
        1001,
        "the Durable Object namespace was deleted",
    );
}

/// An instance of the object ended: the sockets whose listeners lived in it
/// close. Its hibernatable sockets go to the next instance.
pub fn instance_ended(namespace_id: &str, object_id: &str) {
    close_where(
        |key, socket| key.0 == namespace_id && key.1 == object_id && !socket.hibernatable,
        1011,
        "the Durable Object stopped",
    );
}

fn forget(key: &ObjectRef, socket: WebSocketId) {
    let mut sockets = SOCKETS.lock().unwrap();

    if let Some(object) = sockets.get_mut(key) {
        object.remove(&socket);

        if object.is_empty() {
            sockets.remove(key);
        }
    }
}

/// Puts what came on `socket` in the mailbox of the object, and starts the
/// object if it does not run.
fn deliver(
    wake: &Wake,
    object_id: &str,
    socket: WebSocketId,
    incoming: WebSocketIncoming,
    permit: Option<OwnedSemaphorePermit>,
) -> Result<(), String> {
    let reload = wake.reload.clone();
    let load = move |depths| reload(depths);

    super::socket_event(
        &wake.namespace,
        object_id.to_string(),
        SocketEvent {
            socket,
            incoming,
            permit,
        },
        load,
    )
}

/// Moves the frames of one socket: waits for the response that hands it
/// over and for the client, then sends the frames of the object and
/// delivers the frames of the client.
async fn run(
    key: ObjectRef,
    socket: WebSocketId,
    wake: Wake,
    mut out_rx: mpsc::UnboundedReceiver<WebSocketOutgoing>,
    attached: oneshot::Receiver<WebSocketUpgrade>,
) {
    let attach = async { attached.await.ok()?.await.ok() };
    let deadline = tokio::time::sleep(ATTACH_TIMEOUT);
    tokio::pin!(attach, deadline);

    // The frames of the object before the client comes
    let mut waiting = Vec::new();
    let mut waiting_bytes = 0;
    let mut open = true;

    let conn = loop {
        tokio::select! {
            conn = &mut attach => break conn,
            _ = &mut deadline => break None,

            message = out_rx.recv(), if open => {
                let Some(message) = message else {
                    open = false;
                    continue;
                };

                waiting_bytes += match &message {
                    WebSocketOutgoing::Text(text) => text.len(),
                    WebSocketOutgoing::Binary(bytes) => bytes.len(),
                    WebSocketOutgoing::Close { .. } => 0,
                };
                waiting.push(message);

                if waiting_bytes > MAX_QUEUED_BYTES {
                    forget(&key, socket);
                    let closed = WebSocketIncoming::Closed {
                        code: 1008,
                        reason: "The client does not read fast enough".into(),
                    };
                    let _ = deliver(&wake, &key.1, socket, closed, None);
                    return;
                }
            }
        }
    };

    let Some(WebSocketConnection {
        send_tx,
        mut recv_rx,
    }) = conn
    else {
        forget(&key, socket);
        return;
    };

    // The bytes of the frames of the client that the object did not take yet
    let unread = Arc::new(Semaphore::new(MAX_QUEUED_BYTES));

    for message in waiting {
        let closes = matches!(message, WebSocketOutgoing::Close { .. });
        let _ = send_tx.send(message);

        if closes {
            forget(&key, socket);
            return;
        }
    }

    loop {
        tokio::select! {
            message = out_rx.recv() => {
                // The socket left the object, which sent its close first
                let Some(message) = message else { break };
                let closes = matches!(message, WebSocketOutgoing::Close { .. });
                let _ = send_tx.send(message);

                if closes {
                    break;
                }
            }

            incoming = recv_rx.recv() => {
                let incoming = incoming.unwrap_or(WebSocketIncoming::Closed {
                    code: 1006,
                    reason: String::new(),
                });
                let last = matches!(incoming, WebSocketIncoming::Closed { .. } | WebSocketIncoming::Error(_));

                let bytes = match &incoming {
                    WebSocketIncoming::Text(text) => text.len(),
                    WebSocketIncoming::Binary(bytes) => bytes.len(),
                    _ => 0,
                };

                let Ok(permit) = unread.clone().try_acquire_many_owned(bytes as u32) else {
                    let reason = "The object does not take the messages fast enough";
                    forget(&key, socket);
                    let _ = send_tx.send(WebSocketOutgoing::Close { code: 1008, reason: reason.into() });
                    let closed = WebSocketIncoming::Closed { code: 1008, reason: reason.into() };
                    let _ = deliver(&wake, &key.1, socket, closed, None);
                    break;
                };

                if last {
                    forget(&key, socket);
                }

                // An object the runner cannot start loses the client
                if let Err(reason) = deliver(&wake, &key.1, socket, incoming, Some(permit)) {
                    let reason = reason.chars().take(120).collect();
                    let _ = send_tx.send(WebSocketOutgoing::Close { code: 1013, reason });
                    break;
                }

                if last {
                    break;
                }
            }
        }
    }

    forget(&key, socket);
}
