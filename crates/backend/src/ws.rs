//! Single-port server that serves the embedded UI over HTTP **and** streams CLV
//! envelopes over WebSocket to connected clients.
//!
//! [`serve`] binds a `tokio-tungstenite` server to a [`SocketAddr`] and returns a
//! [`BoundServer`] exposing the **actual** bound address (so a test may pass
//! `127.0.0.1:0` and read back the ephemeral port) plus a shutdown handle. Each
//! accepted connection is discriminated non-destructively (P10-1, Design Decision
//! #1): the head is `peek`ed off the intact [`TcpStream`] (bounded to 8 KiB so a
//! hostile client cannot force unbounded buffering), and a request carrying
//! `Upgrade: websocket` is handed **unchanged** to the WebSocket path below while
//! every other `GET` is answered by the embedded static handler ([`serve_http`]):
//! the [`FrontendAssets`] bundle keyed by request path (MIME by extension), with
//! `index.html` as the SPA fallback for extension-less routes and a `404` for a
//! missing asset that names a real extension. A malformed or oversized head is
//! answered with `400` — never a panic. So `lattice <dir>` serves the live-graph UI
//! from one port with no separate frontend process.
//!
//! Each accepted WebSocket connection is handled independently and panic-free: the
//! per-connection task first sends the current graph (root-only) `snapshot`, then — when the graph
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
use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
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

/// The built SvelteKit UI (`frontend/build/`) embedded into the binary (P10-1).
///
/// `rust-embed` interpolates `$CARGO_MANIFEST_DIR` (via the `interpolate-folder-path`
/// feature) to resolve the workspace-relative bundle, embedding it in `--release`
/// and reading it from disk in debug builds. Absent a build, the folder exists (a
/// committed `.gitkeep` + `build.rs`) but is empty, so lookups simply miss.
#[derive(rust_embed::RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../frontend/build"]
struct FrontendAssets;

/// Where the HTTP static handler reads frontend assets from.
///
/// Production serving uses [`AssetSource::Embedded`] (the [`FrontendAssets`] bundle);
/// [`AssetSource::Dir`] points the handler at an on-disk webroot, which lets a test
/// serve a fixture directory (and doubles as a dev mode that serves an un-embedded
/// build straight from disk).
#[derive(Clone)]
enum AssetSource {
    /// The compile-time-embedded `frontend/build/` bundle.
    Embedded,
    /// An on-disk directory served as the webroot. Only the test harness (and a
    /// future dev-serve mode) constructs this, so it is dead in a production build.
    #[cfg_attr(not(test), allow(dead_code))]
    Dir(PathBuf),
}

impl AssetSource {
    /// Loads the asset stored at `key` (a `/`-separated, root-relative path), or
    /// [`None`] when it is absent.
    ///
    /// Never blocks the async runtime and never panics on client-supplied keys: the
    /// embedded lookup (which reads from disk in debug builds) runs on a blocking
    /// thread, and the directory source is confined to its webroot before any read.
    ///
    /// The [`AssetSource::Dir`] guard mirrors `rust-embed`'s own: it normalises
    /// backslashes to `/` (so a Windows-style `..\` cannot slip past), rejects any
    /// component that is not a plain name (a [`std::path::Component::ParentDir`],
    /// root, or drive prefix — the latter two would otherwise let `join` *replace*
    /// the webroot), then canonicalises the joined path and serves it only when it
    /// still `starts_with` the canonical webroot (defeating symlink escapes). All of
    /// this — plus the read — runs on a blocking thread, and any I/O error (missing
    /// file, escape) is treated as a miss.
    async fn load(&self, key: &str) -> Option<Vec<u8>> {
        match self {
            AssetSource::Embedded => {
                let key = key.to_string();
                tokio::task::spawn_blocking(move || {
                    FrontendAssets::get(&key).map(|file| file.data.into_owned())
                })
                .await
                .ok()
                .flatten()
            }
            AssetSource::Dir(root) => {
                let rel = PathBuf::from(key.replace('\\', "/"));
                if rel
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
                {
                    return None;
                }
                let root = root.clone();
                tokio::task::spawn_blocking(move || {
                    let root = root.canonicalize().ok()?;
                    let full = root.join(&rel).canonicalize().ok()?;
                    if full.starts_with(&root) {
                        std::fs::read(full).ok()
                    } else {
                        None
                    }
                })
                .await
                .ok()
                .flatten()
            }
        }
    }
}

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

/// Binds the single-port HTTP + WebSocket server to `addr` and starts serving.
///
/// Binds a [`TcpListener`] to `addr` (pass `127.0.0.1:0` for an ephemeral port),
/// reads back the actual bound [`SocketAddr`], spawns the accept loop, and returns
/// a [`BoundServer`] immediately so a caller can connect without racing the loop.
///
/// Each accepted connection is discriminated by [`dispatch_connection`]: a
/// `Upgrade: websocket` request is served by [`handle_connection`] (first the
/// current `graph` root-only snapshot, then every [`EventEnvelope`] broadcast on
/// `events`, replying to `{"type":"snapshot"}`/`{"type":"expand",...}` frames),
/// while any other `GET` is served the embedded frontend by [`serve_http`]. The
/// frontend comes from [`AssetSource::Embedded`] (the [`FrontendAssets`] bundle);
/// see [`serve_with_assets`] for the directory-backed variant used by tests.
/// `graph` is shared behind a [`Mutex`] so replies reflect concurrent mutations;
/// `events` is the fan-out [`broadcast`] sender the graph publishes on.
///
/// # Errors
/// Returns the [`std::io::Error`] from binding or reading the listener address.
pub async fn serve(
    addr: SocketAddr,
    graph: Arc<Mutex<Graph>>,
    events: broadcast::Sender<EventEnvelope>,
) -> std::io::Result<BoundServer> {
    serve_with_assets(addr, graph, events, AssetSource::Embedded).await
}

/// Binds the server like [`serve`] but serves static assets from `assets`.
///
/// This is [`serve`]'s implementation with the asset source made explicit so tests
/// can point the HTTP handler at a fixture webroot ([`AssetSource::Dir`]) instead of
/// the compile-time [`FrontendAssets`] bundle. Every accepted connection is spawned
/// through [`dispatch_connection`], which peeks the head to route WebSocket upgrades
/// to [`handle_connection`] and all other requests to [`serve_http`].
///
/// # Errors
/// Returns the [`std::io::Error`] from binding or reading the listener address.
async fn serve_with_assets(
    addr: SocketAddr,
    graph: Arc<Mutex<Graph>>,
    events: broadcast::Sender<EventEnvelope>,
    assets: AssetSource,
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
                    let assets = assets.clone();
                    tokio::spawn(async move {
                        let _ = dispatch_connection(stream, graph, events, assets).await;
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

/// Routes one accepted connection to the WebSocket or HTTP path, non-destructively.
///
/// Peeks the request head off the intact `stream` ([`peek_request_head`], bounded to
/// 8 KiB) **without consuming it**, so a `Upgrade: websocket` request can be handed
/// unchanged to [`handle_connection`] (whose `accept_async` re-reads the full head).
/// Every other request is served the embedded frontend by [`serve_http`]. A head
/// that cannot be read (peer closed / socket error) drops the connection silently.
async fn dispatch_connection(
    stream: TcpStream,
    graph: Arc<Mutex<Graph>>,
    events: broadcast::Sender<EventEnvelope>,
    assets: AssetSource,
) -> ConnResult {
    match peek_request_head(&stream).await {
        Some(head) if head_is_websocket_upgrade(&head) => {
            handle_connection(stream, graph, events).await
        }
        Some(head) => serve_http(stream, &head, &assets).await,
        None => Ok(()),
    }
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

/// Reads the HTTP request head off `stream` **without consuming it**, bounded.
///
/// Peeks up to 8 KiB and returns once the head terminator (`\r\n\r\n`) is seen, the
/// cap is reached, or a 2-second budget elapses — so a slow or hostile client can
/// neither stall the handler nor force unbounded buffering (Design Decision #1).
/// Because it only *peeks*, the bytes stay queued for whichever path handles the
/// connection next (`accept_async` for a WebSocket upgrade). Returns [`None`] when
/// the peer closes before sending anything or the socket errors.
async fn peek_request_head(stream: &TcpStream) -> Option<Vec<u8>> {
    /// Upper bound on the head we inspect, so a client cannot make us buffer without
    /// limit while we look for the terminator.
    const MAX_HEAD: usize = 8 * 1024;

    let mut buf = vec![0u8; MAX_HEAD];
    let deadline = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                return match stream.peek(&mut buf).await {
                    Ok(n) if n > 0 => Some(buf[..n].to_vec()),
                    _ => None,
                };
            }
            peeked = stream.peek(&mut buf) => match peeked {
                Ok(0) | Err(_) => return None,
                Ok(n) => {
                    if n >= MAX_HEAD || buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        return Some(buf[..n].to_vec());
                    }
                    // Head not fully arrived yet; yield briefly before re-peeking.
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }
}

/// Returns `true` when the request `head` carries an `Upgrade: websocket` header.
///
/// Parses each header line case-insensitively (tolerating arbitrary spacing and
/// header casing), so a genuine WebSocket handshake is routed to the untouched
/// `accept_async` path. Panic-free: non-UTF-8 or malformed heads yield `false`.
fn head_is_websocket_upgrade(head: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(head) else {
        return false;
    };
    text.split("\r\n").any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("upgrade")
                && value.to_ascii_lowercase().contains("websocket")
        })
    })
}

/// Extracts the request target (path + query) from a `GET` request `head`.
///
/// Returns [`Some`] only for a well-formed `GET` whose target starts with `/`;
/// any other method, a non-UTF-8 head, or a missing/relative target yields [`None`]
/// (the caller answers such requests with `400`). Panic-free on client bytes.
fn request_target(head: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(head).ok()?;
    let request_line = text.split("\r\n").next()?;
    let mut parts = request_line.split(' ');
    if !parts.next()?.eq_ignore_ascii_case("GET") {
        return None;
    }
    let target = parts.next()?;
    target.starts_with('/').then(|| target.to_string())
}

/// Maps a request target to its embedded/on-disk asset key.
///
/// Strips the query/fragment and the leading `/`; a bare `/` maps to `index.html`.
fn asset_key(target: &str) -> String {
    let path = target.split(['?', '#']).next().unwrap_or(target);
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        "index.html".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Returns `true` when the final path segment of `key` names a file extension.
///
/// Extension-less targets (e.g. `/some/deep/link`) are SPA client routes served the
/// `index.html` shell; a target with an extension is looked up as a real asset.
fn key_has_extension(key: &str) -> bool {
    key.rsplit('/').next().is_some_and(|seg| seg.contains('.'))
}

/// Returns the `Content-Type` for `key`, by file extension.
///
/// Covers the extensions a SvelteKit static build emits; anything unrecognised
/// falls back to `application/octet-stream`.
fn mime_for_path(key: &str) -> &'static str {
    let ext = key.rsplit('.').next().unwrap_or_default();
    match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "wasm" => "application/wasm",
        "ico" => "image/x-icon",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Serves the embedded frontend for one non-WebSocket HTTP request.
///
/// Resolves the `head`'s target to an asset key: a key with a real extension is
/// served with its [`mime_for_path`] type or answered `404` when absent, while an
/// extension-less route falls back to the `index.html` SPA shell. A head with no
/// parseable `GET` request line is answered `400`. Never panics on client bytes.
async fn serve_http(mut stream: TcpStream, head: &[u8], assets: &AssetSource) -> ConnResult {
    // The head was only *peeked* (left in the socket so the WebSocket path could
    // re-read it). On the HTTP path we consume those bytes now: dropping a stream
    // that still holds unread received data triggers a TCP RST on some platforms,
    // which a client would observe as a connection reset instead of our response.
    //
    // The peek is capped at 8 KiB, so `head.len()` can be short of the bytes the
    // client actually sent (an over-cap head, or a body). Draining exactly
    // `head.len()` would leave that residue queued and risk the very RST above, so
    // instead drain non-blocking until the socket would block or reaches EOF —
    // bounded by `MAX_DRAIN` so a client that streams forever can't pin us here.
    const MAX_DRAIN: usize = 64 * 1024;
    let mut scratch = [0u8; 4096];
    let mut drained = 0usize;
    while drained < MAX_DRAIN {
        match stream.try_read(&mut scratch) {
            // EOF: peer closed its write half, nothing more will arrive.
            Ok(0) => break,
            Ok(n) => drained += n,
            // Any error stops the drain: `WouldBlock` means the buffered head is
            // cleared (nothing more is ready, and we must not block waiting for
            // bytes that may never come), and a real socket error is surfaced by
            // the write below. Both cases end the loop.
            Err(_) => break,
        }
    }

    let Some(target) = request_target(head) else {
        return write_response(
            &mut stream,
            400,
            "text/plain; charset=utf-8",
            b"Bad Request",
        )
        .await;
    };

    let key = asset_key(&target);
    let lookup = if key_has_extension(&key) {
        key.as_str()
    } else {
        // SPA fallback: an unknown extension-less route renders the app shell.
        "index.html"
    };

    match assets.load(lookup).await {
        Some(body) => write_response(&mut stream, 200, mime_for_path(lookup), &body).await,
        None => write_response(&mut stream, 404, "text/plain; charset=utf-8", b"Not Found").await,
    }
}

/// Writes a minimal HTTP/1.1 response (`Connection: close`) and flushes it.
///
/// Emits the status line, `Content-Type`, an `X-Content-Type-Options: nosniff`
/// guard, `Content-Length`, and body, then flushes; closing the connection after the
/// body lets a plain HTTP client observe the end of the response via EOF. The
/// `nosniff` header stops a browser MIME-sniffing a response served with the
/// `application/octet-stream` fallback (an unknown extension) into an executable type.
async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> ConnResult {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        len = body.len(),
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clv::ClvEvent;
    use crate::wire::{EventType, Node, NodeStatus, NodeType, Payload};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
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

    // ---- P10-1: single-port HTTP static-asset serving alongside WS ----
    //
    // RED until P10-1 lands: neither `serve_with_assets` nor `AssetSource` (nor the
    // `mime_for_path` / `request_target` / `head_is_websocket_upgrade` helpers)
    // exists, so the whole test binary fails to compile — the HTTP-serving path and
    // the rust-embed static handler have not been written yet.

    /// A minimal HTTP/1.1 response parsed off a raw socket read: the status code,
    /// every header as a `(name, value)` pair, and the body.
    struct HttpResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl HttpResponse {
        /// Case-insensitive lookup of the first value for header `name`.
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }

        /// The `Content-Type` header value, if present (case-insensitive).
        fn content_type(&self) -> Option<&str> {
            self.header("content-type")
        }
    }

    /// Parses a raw HTTP/1.1 response buffer into its status/headers/body.
    fn parse_http_response(raw: &[u8]) -> HttpResponse {
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("headers terminator");
        let head = std::str::from_utf8(&raw[..split]).expect("ascii head");
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split(' ').nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .expect("status code");
        let headers = lines
            .filter_map(|l| {
                l.split_once(':')
                    .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
            })
            .collect();
        HttpResponse {
            status,
            headers,
            body: raw[split + 4..].to_vec(),
        }
    }

    /// Issues a bare `GET <path>` over a fresh TCP connection and returns the
    /// parsed response. `Connection: close` lets `read_to_end` observe EOF.
    async fn http_get(addr: SocketAddr, path: &str) -> HttpResponse {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.expect("write");
        let mut raw = Vec::new();
        timeout(RECV_TIMEOUT, stream.read_to_end(&mut raw))
            .await
            .expect("response within timeout")
            .expect("read response");
        parse_http_response(&raw)
    }

    /// Writes each `(relative-path, contents)` fixture into a fresh `TempDir` and
    /// starts a server pointed at it via [`AssetSource::Dir`]. The returned
    /// `TempDir` must be held for the server's lifetime (drop deletes the webroot).
    async fn start_with_webroot(files: &[(&str, &str)]) -> (BoundServer, TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (rel, contents) in files {
            let path = dir.path().join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(&path, contents).expect("write fixture");
        }
        let graph = Arc::new(Mutex::new(Graph::new()));
        let (tx, _rx) = broadcast::channel(16);
        let server = serve_with_assets(
            "127.0.0.1:0".parse().unwrap(),
            graph,
            tx,
            AssetSource::Dir(dir.path().to_path_buf()),
        )
        .await
        .expect("server binds");
        (server, dir)
    }

    #[tokio::test]
    async fn http_get_root_serves_index_html() {
        let (server, _dir) =
            start_with_webroot(&[("index.html", "<!doctype html><title>Lattice</title>")]).await;

        let resp = http_get(server.addr, "/").await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type(), Some("text/html; charset=utf-8"));
        assert!(
            String::from_utf8_lossy(&resp.body).contains("Lattice"),
            "index body is served, got {:?}",
            String::from_utf8_lossy(&resp.body)
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn http_get_asset_serves_javascript_mime() {
        let (server, _dir) = start_with_webroot(&[
            ("index.html", "<html></html>"),
            ("assets/app.js", "export const x = 1;"),
        ])
        .await;

        let resp = http_get(server.addr, "/assets/app.js").await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type(), Some("text/javascript; charset=utf-8"));
        assert_eq!(resp.body, b"export const x = 1;");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn http_get_spa_route_falls_back_to_index() {
        let (server, _dir) = start_with_webroot(&[("index.html", "<html>spa-shell</html>")]).await;

        let resp = http_get(server.addr, "/some/deep/link").await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type(), Some("text/html; charset=utf-8"));
        assert!(String::from_utf8_lossy(&resp.body).contains("spa-shell"));

        server.shutdown().await;
    }

    #[tokio::test]
    async fn http_get_missing_asset_with_extension_returns_404() {
        let (server, _dir) = start_with_webroot(&[("index.html", "<html></html>")]).await;

        let resp = http_get(server.addr, "/nope.css").await;
        assert_eq!(resp.status, 404);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn websocket_upgrade_still_completes_through_discrimination() {
        let (server, _dir) = start_with_webroot(&[("index.html", "<html></html>")]).await;

        // The same address that serves HTTP must still hand a WS upgrade to the
        // untouched `accept_async` path and deliver the root snapshot.
        let mut ws = connect(server.addr).await;
        let env = next_envelope(&mut ws).await;
        assert_eq!(env.event_type, EventType::Snapshot);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn malformed_oversized_head_does_not_panic() {
        let (server, _dir) = start_with_webroot(&[("index.html", "<html>ok</html>")]).await;

        // A garbage, oversized, CRLF-free head must be handled bounded + panic-free.
        let mut junk = TcpStream::connect(server.addr).await.expect("connect");
        let blob = vec![b'x'; 32 * 1024];
        let _ = junk.write_all(&blob).await;
        let _ = junk.shutdown().await;
        drop(junk);

        // The server survived and still serves a normal request.
        let resp = http_get(server.addr, "/").await;
        assert_eq!(resp.status, 200);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn http_head_arriving_in_pieces_is_served() {
        let (server, _dir) = start_with_webroot(&[("index.html", "<html>piecewise</html>")]).await;

        // Send the head in two pieces so the peek loop must retry before the
        // `\r\n\r\n` terminator arrives.
        let mut stream = TcpStream::connect(server.addr).await.expect("connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
            .await
            .expect("write part 1");
        tokio::time::sleep(Duration::from_millis(30)).await;
        stream
            .write_all(b"Connection: close\r\n\r\n")
            .await
            .expect("write part 2");

        let mut raw = Vec::new();
        timeout(RECV_TIMEOUT, stream.read_to_end(&mut raw))
            .await
            .expect("response within timeout")
            .expect("read response");
        let resp = parse_http_response(&raw);
        assert_eq!(resp.status, 200);
        assert!(String::from_utf8_lossy(&resp.body).contains("piecewise"));

        server.shutdown().await;
    }

    #[tokio::test]
    async fn connection_closed_before_any_bytes_is_dropped_cleanly() {
        let (server, _dir) = start_with_webroot(&[("index.html", "<html>ok</html>")]).await;

        // Open then immediately close without sending a request head: the peek sees
        // EOF and the connection is dropped without panicking.
        let closed = TcpStream::connect(server.addr).await.expect("connect");
        drop(closed);

        // The server is unaffected and still serves normal requests.
        let resp = http_get(server.addr, "/").await;
        assert_eq!(resp.status, 200);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn embedded_asset_source_is_wired_panic_free() {
        // The default `serve` uses `AssetSource::Embedded`; against an empty
        // `frontend/build/` (only `.gitkeep`) a real-extension miss is a clean 404,
        // exercising the embedded lookup path without panicking.
        let (server, _tx) = start().await;

        let resp = http_get(server.addr, "/definitely-absent.css").await;
        assert_eq!(resp.status, 404);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn dir_asset_source_confines_to_webroot() {
        // A webroot with a legit nested asset, plus a secret file that lives one
        // level *above* the webroot — the target a traversal would try to reach.
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("webroot");
        std::fs::create_dir_all(root.join("assets")).expect("mkdir assets");
        std::fs::write(root.join("assets/app.js"), b"legit").expect("write asset");
        std::fs::write(base.path().join("secret.txt"), b"secret").expect("write secret");

        let source = AssetSource::Dir(root.clone());

        // A genuine nested asset inside the webroot is still served.
        assert_eq!(
            source.load("assets/app.js").await.as_deref(),
            Some(&b"legit"[..])
        );

        // `..`-component traversal to the sibling secret is refused.
        assert!(
            source.load("../secret.txt").await.is_none(),
            "parent-dir escape"
        );
        // Backslash-normalised traversal (Windows-style `..\`) is refused: the raw
        // string `..\secret.txt` normalises to a `..` component, not the literal name.
        assert!(
            source.load("..\\secret.txt").await.is_none(),
            "backslash escape"
        );
        assert!(
            source.load("assets\\..\\..\\secret.txt").await.is_none(),
            "mixed backslash traversal"
        );
        // An absolute / drive-style path that `join` would otherwise let *replace*
        // the webroot is refused (root/prefix components).
        assert!(source.load("/etc/hosts").await.is_none(), "absolute path");
        assert!(
            source.load("/C:/Windows/system32").await.is_none(),
            "drive path"
        );

        // A symlink *inside* the webroot pointing *outside* it passes the component
        // check (its key is a plain name) but is refused by the canonicalise +
        // `starts_with` confinement — symlinks are not followed out of the webroot.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(base.path().join("secret.txt"), root.join("escape"))
                .expect("symlink");
            assert!(source.load("escape").await.is_none(), "symlink escape");
        }
    }

    #[tokio::test]
    async fn http_responses_carry_nosniff_header() {
        let (server, _dir) = start_with_webroot(&[("index.html", "<html>ok</html>")]).await;

        let resp = http_get(server.addr, "/").await;
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.header("x-content-type-options"),
            Some("nosniff"),
            "static responses must set nosniff so an octet-stream fallback isn't MIME-sniffed"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn http_head_exceeding_peek_cap_is_served_without_reset() {
        let (server, _dir) = start_with_webroot(&[("index.html", "<html>big-head</html>")]).await;

        // A valid request line followed by a padding header that pushes the head
        // well past the 8 KiB peek cap, so the drain must clear more than the peeked
        // `head.len()` bytes. Half-closing the write side lets the drain see EOF, so
        // no residue survives to RST-truncate the response.
        let mut stream = TcpStream::connect(server.addr).await.expect("connect");
        let padding = "a".repeat(16 * 1024);
        let req = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nX-Pad: {padding}\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(req.as_bytes())
            .await
            .expect("write big head");
        stream.shutdown().await.expect("half-close write");

        let mut raw = Vec::new();
        timeout(RECV_TIMEOUT, stream.read_to_end(&mut raw))
            .await
            .expect("response within timeout")
            .expect("read response without reset");
        let resp = parse_http_response(&raw);
        assert_eq!(resp.status, 200);
        assert!(String::from_utf8_lossy(&resp.body).contains("big-head"));

        server.shutdown().await;
    }

    #[test]
    fn mime_for_path_maps_known_extensions() {
        let cases = [
            ("index.html", "text/html; charset=utf-8"),
            ("assets/app.js", "text/javascript; charset=utf-8"),
            ("app.mjs", "text/javascript; charset=utf-8"),
            ("style.css", "text/css; charset=utf-8"),
            ("data.json", "application/json"),
            ("icon.svg", "image/svg+xml"),
            ("bin.wasm", "application/wasm"),
            ("mystery.xyz", "application/octet-stream"),
        ];
        for (input, want) in cases {
            assert_eq!(mime_for_path(input), want, "input: {input}");
        }
    }

    #[test]
    fn request_target_parses_only_get_requests() {
        let get = b"GET /assets/app.js?v=1 HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(request_target(get).as_deref(), Some("/assets/app.js?v=1"));
        let post: &[u8] = b"POST / HTTP/1.1\r\n\r\n";
        assert_eq!(request_target(post), None);
        let junk: &[u8] = b"\xff\xfe not http";
        assert_eq!(request_target(junk), None);
    }

    #[test]
    fn head_is_websocket_upgrade_detects_the_upgrade_header() {
        let ws = b"GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
        assert!(head_is_websocket_upgrade(ws));
        let ws_ci = b"GET / HTTP/1.1\r\nupgrade:  WebSocket \r\n\r\n";
        assert!(head_is_websocket_upgrade(ws_ci));
        let plain = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(!head_is_websocket_upgrade(plain));
        let non_utf8: &[u8] = b"\xff\xfe upgrade: websocket";
        assert!(!head_is_websocket_upgrade(non_utf8));
    }
}
