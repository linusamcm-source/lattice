//! WebSocket server that streams CLV envelopes to connected clients.
//!
//! [`serve`] binds a `tokio-tungstenite` server to a [`SocketAddr`] and returns a
//! [`BoundServer`] exposing the **actual** bound address (so a test may pass
//! `127.0.0.1:0` and read back the ephemeral port) plus a shutdown handle. Each
//! accepted connection is handled independently and panic-free: the per-connection
//! task first sends the current graph (root-only) `snapshot`, then — when the graph
//! has a non-empty roster — an `agent.roster` trailer carrying the live agent layer
//! (P9-7, [`Graph::roster_snapshot`](crate::graph::Graph::roster_snapshot)), so a
//! mid-run connect sees agent nodes *and* their roster. It then forwards every
//! [`EventEnvelope`](crate::wire::EventEnvelope) published on the shared
//! [`broadcast`] channel as JSON text, while concurrently honouring two client
//! requests — `{"type":"snapshot"}` with a fresh snapshot (again trailed by the
//! roster when non-empty) and `{"type":"expand","nodeId":...}` with that node's
//! `subtree` (its direct children), the Phase-1 lazy-hierarchy load path.
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
/// current `graph` (root-only) snapshot, then every [`EventEnvelope`] broadcast on
/// `events`; a client `{"type":"snapshot"}` frame triggers a fresh snapshot reply
/// and a `{"type":"expand","nodeId":...}` frame triggers that node's `subtree`
/// reply. `graph` is shared behind a [`Mutex`] so replies reflect concurrent
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
/// in the fan-out and will miss no subsequent broadcast), then — when the graph has
/// a non-empty roster — sends an `agent.roster` trailer built from
/// [`Graph::roster_snapshot`] (P9-7), so the connect delivers the agent layer as
/// well as the tree. It then loops: forwarding each broadcast [`EventEnvelope`] as
/// JSON text and replying to client requests — `{"type":"snapshot"}` with a fresh
/// root-only snapshot **followed by the same roster trailer** when non-empty, and
/// `{"type":"expand","nodeId":...}` with that node's `subtree`. Returns when the
/// client closes or errors, or the broadcast channel closes. Lagged broadcasts are
/// skipped rather than fatal.
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

    // P9-7: trail the root-only snapshot with the live roster (Design Decision #4)
    // when the graph has rostered agents, so a mid-run connect sees agent nodes
    // *and* their roster rather than an empty one. Benign duplicate: if an activity
    // lands between `subscribe` (above) and this read, the client may receive the
    // roster once here and again as the buffered broadcast — harmless, since the
    // frontend reducer replaces the roster wholesale.
    let roster = graph.lock().await.roster_snapshot();
    if let Some(roster) = roster {
        write
            .send(Message::text(serde_json::to_string(&roster)?))
            .await?;
    }

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
                    if let Ok(text) = msg.to_text() {
                        if is_snapshot_request(text) {
                            let snapshot = graph.lock().await.snapshot();
                            write
                                .send(Message::text(serde_json::to_string(&snapshot)?))
                                .await?;
                            // P9-7: a resync repeats the connect-time snapshot→roster
                            // trailer pair, so a reconnecting client re-hydrates the
                            // agent layer with the current roster.
                            let roster = graph.lock().await.roster_snapshot();
                            if let Some(roster) = roster {
                                write
                                    .send(Message::text(serde_json::to_string(&roster)?))
                                    .await?;
                            }
                        } else if let Some(node_id) = expand_request_node_id(text) {
                            let subtree = graph.lock().await.subtree(&node_id);
                            write
                                .send(Message::text(serde_json::to_string(&subtree)?))
                                .await?;
                        }
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

/// Returns the requested node id when `text` is a client `expand` request.
///
/// The Phase-1 client lazily loads a node's direct children by sending the JSON
/// text `{"type":"expand","nodeId":"<id>"}` (mirroring the `{"type":"snapshot"}`
/// resync request); the server replies with that node's `subtree`. Returns
/// `Some(node_id)` only for a well-formed request. Parsing is panic-free: any
/// non-JSON, non-object, `type != "expand"`, or missing/non-string `nodeId`
/// yields `None`.
fn expand_request_node_id(text: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str)? != "expand" {
        return None;
    }
    value
        .get("nodeId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clv::ClvEvent;
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

    /// A roster-carrying variant of [`start`]: seeds one active agent into the
    /// graph's roster via the Phase-8 activity path (`apply_clv`) **before**
    /// serving, so a connecting client exercises the P9-7 snapshot+roster trailer.
    async fn start_with_roster() -> (BoundServer, broadcast::Sender<EventEnvelope>) {
        let mut graph = Graph::new();
        let _ = graph.apply_clv(&ClvEvent::Activity {
            session: "s1".to_string(),
            pid: Some(48213),
            agent: Some("tdd-green".to_string()),
            msg: Some("touched".to_string()),
            node: "fn:src/x.rs:foo".to_string(),
            action: "modified".to_string(),
        });
        let graph = Arc::new(Mutex::new(graph));
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

    #[test]
    fn expand_request_node_id_extracts_only_well_formed_expand() {
        let cases = [
            (
                "{\"type\":\"expand\",\"nodeId\":\"file:a.rs\"}",
                Some("file:a.rs"),
            ),
            ("{\"type\":\"snapshot\"}", None),
            ("{\"type\":\"expand\"}", None),
            ("{\"nodeId\":\"file:a.rs\"}", None),
            ("not json", None),
            ("{}", None),
            ("[]", None),
        ];
        for (input, want) in cases {
            assert_eq!(
                expand_request_node_id(input).as_deref(),
                want,
                "input: {input}"
            );
        }
    }

    #[tokio::test]
    async fn expand_request_yields_a_subtree_of_direct_children() {
        let mut graph = Graph::new();
        graph.upsert_node(Node {
            id: "file:src/x.rs".to_string(),
            node_type: NodeType::File,
            label: "x.rs".to_string(),
            parent_id: None,
            child_ids: Vec::new(),
            status: NodeStatus::Unknown,
            docs: None,
            signature: None,
            meta: None,
        });
        graph.upsert_node(Node {
            id: "fn:src/x.rs:f".to_string(),
            node_type: NodeType::Function,
            label: "f".to_string(),
            parent_id: Some("file:src/x.rs".to_string()),
            child_ids: Vec::new(),
            status: NodeStatus::Unknown,
            docs: None,
            signature: None,
            meta: None,
        });
        let graph = Arc::new(Mutex::new(graph));
        let (tx, _rx) = broadcast::channel(16);
        let server = serve("127.0.0.1:0".parse().unwrap(), graph, tx)
            .await
            .expect("server binds");
        let mut ws = connect(server.addr).await;

        // Drain the lazy snapshot first.
        let snapshot = next_envelope(&mut ws).await;
        assert_eq!(snapshot.event_type, EventType::Snapshot);

        ws.send(Message::text(
            "{\"type\":\"expand\",\"nodeId\":\"file:src/x.rs\"}",
        ))
        .await
        .expect("send expand");

        let env = next_envelope(&mut ws).await;
        assert_eq!(env.event_type, EventType::Subtree);
        match env.payload {
            Payload::Subtree {
                parent_id, nodes, ..
            } => {
                assert_eq!(parent_id, "file:src/x.rs");
                assert!(
                    nodes.iter().any(|n| n.id == "fn:src/x.rs:f"),
                    "subtree must include the direct function child: {nodes:?}"
                );
            }
            other => panic!("expected subtree payload, got {other:?}"),
        }

        server.shutdown().await;
    }

    // ---- P9-7: snapshot/resync carries roster state (Design Decision #4) ----
    //
    // RED until P9-7 lands: `handle_connection` sends only the root-only snapshot,
    // never the `agent.roster` trailer, so `connect_sends_snapshot_then_roster*`
    // and the resync variant fail (the roster trailer is absent). The whole test
    // binary is additionally blocked at compile time by the missing
    // `Graph::roster_snapshot` in the graph.rs P9-7 tests (same lib crate).

    /// On connect against a non-empty roster the client receives the `snapshot`
    /// first, then an `agent.roster` trailer carrying every seeded roster entry.
    #[tokio::test]
    async fn connect_sends_snapshot_then_roster_when_roster_non_empty() {
        let (server, _tx) = start_with_roster().await;
        let mut ws = connect(server.addr).await;

        let first = next_envelope(&mut ws).await;
        assert_eq!(
            first.event_type,
            EventType::Snapshot,
            "the first message is the snapshot, got {first:?}"
        );

        let second = next_envelope(&mut ws).await;
        assert_eq!(
            second.event_type,
            EventType::AgentRoster,
            "the second message is the agent.roster trailer, got {second:?}"
        );
        match second.payload {
            Payload::AgentRoster { agents, .. } => assert!(
                agents
                    .iter()
                    .any(|a| a.agent_id == "tdd-green" && a.process_id == 48213),
                "the trailer carries every seeded roster entry, got {agents:?}"
            ),
            other => panic!("expected agent.roster payload, got {other:?}"),
        }

        server.shutdown().await;
    }

    /// Regression: an empty roster emits **no** trailing `agent.roster` — the
    /// message after the snapshot is the normal broadcast, never a spurious roster.
    #[tokio::test]
    async fn connect_against_empty_roster_sends_no_roster_trailer() {
        let (server, tx) = start().await;
        let mut ws = connect(server.addr).await;

        let first = next_envelope(&mut ws).await;
        assert_eq!(
            first.event_type,
            EventType::Snapshot,
            "the first message is the snapshot, got {first:?}"
        );

        // With no roster to trail, the next message must be the broadcast we
        // publish — never a spurious empty agent.roster.
        let id = "fn:src/x.rs:foo";
        tx.send(node_upsert_envelope(id)).expect("publish");

        let next = next_envelope(&mut ws).await;
        assert_eq!(
            next.event_type,
            EventType::NodeUpsert,
            "an empty roster must not emit a trailing agent.roster, got {next:?}"
        );

        server.shutdown().await;
    }

    /// A `{"type":"snapshot"}` resync repeats the snapshot-then-roster pair when
    /// the roster is non-empty.
    #[tokio::test]
    async fn resync_request_sends_snapshot_then_roster_when_roster_non_empty() {
        let (server, _tx) = start_with_roster().await;
        let mut ws = connect(server.addr).await;

        // Drain the initial connect snapshot + roster trailer.
        let first = next_envelope(&mut ws).await;
        assert_eq!(first.event_type, EventType::Snapshot);
        let trailer = next_envelope(&mut ws).await;
        assert_eq!(trailer.event_type, EventType::AgentRoster);

        // The resync request must repeat the same ordered pair.
        ws.send(Message::text("{\"type\":\"snapshot\"}"))
            .await
            .expect("send request");

        let again = next_envelope(&mut ws).await;
        assert_eq!(
            again.event_type,
            EventType::Snapshot,
            "resync replies with a snapshot first, got {again:?}"
        );
        let again_roster = next_envelope(&mut ws).await;
        assert_eq!(
            again_roster.event_type,
            EventType::AgentRoster,
            "resync trails the snapshot with the roster, got {again_roster:?}"
        );
        match again_roster.payload {
            Payload::AgentRoster { agents, .. } => assert!(
                agents
                    .iter()
                    .any(|a| a.agent_id == "tdd-green" && a.process_id == 48213),
                "resync roster carries every seeded entry, got {agents:?}"
            ),
            other => panic!("expected agent.roster payload, got {other:?}"),
        }

        server.shutdown().await;
    }
}
