//! WebSocket server that streams CLV envelopes to connected clients.
//!
//! [`serve`] binds a `tokio-tungstenite` server to a [`SocketAddr`] and returns a
//! [`BoundServer`] exposing the **actual** bound address (so a test may pass
//! `127.0.0.1:0` and read back the ephemeral port) plus a shutdown handle. Each
//! accepted connection is handled independently and panic-free: the per-connection
//! task first sends the current graph `snapshot`, then forwards every
//! [`EventEnvelope`](crate::wire::EventEnvelope) published on the shared
//! [`broadcast`] channel as JSON text, while concurrently honouring a client
//! `{"type":"snapshot"}` request with a fresh snapshot.
//!
//! Per `AGENT_PROTOCOL.md` §6 nothing here unwraps on bad input: a client that
//! closes, errors, or lags simply drops out of the fan-out without disturbing the
//! server or its peers.

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::graph::Graph;
use crate::wire::EventEnvelope;

/// Errors raised inside a per-connection handler, used only for `?` propagation.
///
/// The accept loop discards a handler's result, so a connection that hits any of
/// these simply ends; the type exists so serialisation and socket failures can be
/// propagated with `?` instead of unwrapped.
type ConnResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// A running WebSocket server bound to a concrete address.
///
/// `addr` is the **actual** socket the accept loop listens on — when [`serve`] is
/// given `127.0.0.1:0` this is the OS-assigned ephemeral port, which a test reads
/// to connect. Dropping a `BoundServer` (or calling [`BoundServer::shutdown`])
/// signals the accept loop to stop; in-flight connection tasks then wind down on
/// their own.
pub struct BoundServer {
    /// The concrete address the accept loop is listening on.
    pub addr: SocketAddr,
    /// Drop/​send this to ask the accept loop to stop.
    shutdown: oneshot::Sender<()>,
    /// Join handle for the accept loop task.
    handle: JoinHandle<()>,
}

impl BoundServer {
    /// Stops the accept loop and waits for it to finish.
    ///
    /// Sends the shutdown signal, then awaits the accept-loop task so teardown is
    /// deterministic for a caller (or test) that wants to know the listener is
    /// gone. Both steps are best-effort: a loop that already exited is not an
    /// error.
    pub async fn shutdown(self) {
        let BoundServer {
            shutdown, handle, ..
        } = self;
        let _ = shutdown.send(());
        let _ = handle.await;
    }
}

/// Binds a WebSocket server to `addr` and starts streaming CLV envelopes.
///
/// Binds a [`TcpListener`] to `addr` (pass `127.0.0.1:0` for an ephemeral port),
/// reads back the actual bound [`SocketAddr`], spawns the accept loop, and returns
/// a [`BoundServer`] immediately so a caller can connect without racing the loop.
///
/// Each accepted client is served by [`handle_connection`]: it is first sent the
/// current `graph` snapshot, then every [`EventEnvelope`] broadcast on `events`,
/// and a client `{"type":"snapshot"}` text frame triggers a fresh snapshot reply.
/// `graph` is shared behind a [`Mutex`] so the snapshot reflects concurrent
/// mutations; `events` is the fan-out [`broadcast`] sender the graph publishes on.
///
/// # Errors
/// Returns the [`std::io::Error`] from binding or reading the listener address.
pub async fn serve(
    addr: SocketAddr,
    graph: Arc<Mutex<Graph>>,
    events: broadcast::Sender<EventEnvelope>,
) -> std::io::Result<BoundServer> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _peer)) = accepted else { continue };
                    let graph = Arc::clone(&graph);
                    let events = events.clone();
                    tokio::spawn(async move {
                        let _ = handle_connection(stream, graph, events).await;
                    });
                }
            }
        }
    });

    Ok(BoundServer {
        addr: bound,
        shutdown: shutdown_tx,
        handle,
    })
}

/// Serves one accepted TCP connection as a WebSocket CLV stream.
///
/// Upgrades `stream` to a WebSocket, subscribes to `events` **before** sending the
/// initial snapshot (so a client that has received the snapshot is guaranteed to be
/// in the fan-out and will miss no subsequent broadcast), then loops: forwarding
/// each broadcast [`EventEnvelope`] as JSON text and replying to a client
/// `{"type":"snapshot"}` request with a fresh snapshot. Returns when the client
/// closes or errors, or the broadcast channel closes. Lagged broadcasts are skipped
/// rather than fatal.
async fn handle_connection(
    stream: tokio::net::TcpStream,
    graph: Arc<Mutex<Graph>>,
    events: broadcast::Sender<EventEnvelope>,
) -> ConnResult {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut write, mut read) = ws.split();
    let mut rx = events.subscribe();

    let snapshot = graph.lock().await.snapshot();
    write
        .send(Message::text(serde_json::to_string(&snapshot)?))
        .await?;

    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(env) => {
                    write.send(Message::text(serde_json::to_string(&env)?)).await?;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            inbound = read.next() => match inbound {
                Some(Ok(msg)) => {
                    if msg.is_close() {
                        break;
                    }
                    let wants_snapshot = msg.to_text().map(is_snapshot_request).unwrap_or(false);
                    if wants_snapshot {
                        let snapshot = graph.lock().await.snapshot();
                        write
                            .send(Message::text(serde_json::to_string(&snapshot)?))
                            .await?;
                    }
                }
                Some(Err(_)) | None => break,
            },
        }
    }

    Ok(())
}

/// Returns `true` when `text` is a client request for a fresh snapshot.
///
/// The Phase-0 client asks for a resync with the JSON text `{"type":"snapshot"}`.
/// Parsing is panic-free: any non-JSON, non-object, or non-matching `type` simply
/// yields `false`.
fn is_snapshot_request(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(serde_json::Value::as_str)
                .map(|t| t == "snapshot")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{EventType, Node, NodeStatus, NodeType, Payload};
    use std::time::Duration;
    use tokio::time::timeout;
    use tokio_tungstenite::connect_async;

    /// Generous loopback budget so the TCP/WS handshake never flakes under load.
    const RECV_TIMEOUT: Duration = Duration::from_secs(5);

    fn function_node(id: &str, label: &str) -> Node {
        Node {
            id: id.to_string(),
            node_type: NodeType::Function,
            label: label.to_string(),
            parent_id: None,
            child_ids: Vec::new(),
            status: NodeStatus::Unknown,
            docs: None,
            signature: None,
            meta: None,
        }
    }

    fn node_upsert_envelope(id: &str) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-28T00:00:00Z".to_string(),
            session_id: "sess-test".to_string(),
            event_type: EventType::NodeUpsert,
            payload: Payload::NodeUpsert {
                node: function_node(id, "foo"),
            },
        }
    }

    async fn start() -> (BoundServer, broadcast::Sender<EventEnvelope>) {
        let graph = Arc::new(Mutex::new(Graph::new()));
        let (tx, _rx) = broadcast::channel(16);
        let server = serve("127.0.0.1:0".parse().unwrap(), graph, tx.clone())
            .await
            .expect("server binds");
        (server, tx)
    }

    async fn connect(
        addr: SocketAddr,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let (ws, _resp) = timeout(RECV_TIMEOUT, connect_async(format!("ws://{addr}/")))
            .await
            .expect("connect within timeout")
            .expect("handshake succeeds");
        ws
    }

    async fn next_envelope<S>(ws: &mut S) -> EventEnvelope
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        let msg = timeout(RECV_TIMEOUT, ws.next())
            .await
            .expect("message within timeout")
            .expect("stream not ended")
            .expect("frame ok");
        serde_json::from_str(msg.to_text().expect("text frame")).expect("valid envelope")
    }

    #[tokio::test]
    async fn first_message_is_a_snapshot() {
        let (server, _tx) = start().await;
        let mut ws = connect(server.addr).await;

        let env = next_envelope(&mut ws).await;
        assert_eq!(env.event_type, EventType::Snapshot);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn broadcast_node_upsert_reaches_client() {
        let (server, tx) = start().await;
        let mut ws = connect(server.addr).await;

        // Receiving the snapshot proves the handler has already subscribed.
        let first = next_envelope(&mut ws).await;
        assert_eq!(first.event_type, EventType::Snapshot);

        let id = "fn:src/x.rs:foo";
        tx.send(node_upsert_envelope(id)).expect("publish");

        let env = next_envelope(&mut ws).await;
        assert_eq!(env.event_type, EventType::NodeUpsert);
        match env.payload {
            Payload::NodeUpsert { node } => assert_eq!(node.id, id),
            other => panic!("expected node.upsert payload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn snapshot_request_yields_a_snapshot() {
        let (server, _tx) = start().await;
        let mut ws = connect(server.addr).await;

        let first = next_envelope(&mut ws).await;
        assert_eq!(first.event_type, EventType::Snapshot);

        ws.send(Message::text("{\"type\":\"snapshot\"}"))
            .await
            .expect("send request");

        let again = next_envelope(&mut ws).await;
        assert_eq!(again.event_type, EventType::Snapshot);
    }

    #[test]
    fn is_snapshot_request_matches_only_the_snapshot_type() {
        let cases = [
            ("{\"type\":\"snapshot\"}", true),
            ("{\"type\":\"node.upsert\"}", false),
            ("not json", false),
            ("{}", false),
            ("[]", false),
        ];
        for (input, want) in cases {
            assert_eq!(is_snapshot_request(input), want, "input: {input}");
        }
    }
}
