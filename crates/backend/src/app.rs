//! Application wiring: watcher → parser → graph → WebSocket server.
//!
//! [`run`] is the Phase-0 backend entry point used by the binary and by the
//! integration tests. It does the initial parse of a repository tree into a
//! [`Graph`], starts the [`ws`](crate::ws) server, and spawns a watcher pump that
//! re-parses each changed source file (Rust, Python, or TypeScript — see
//! [`parse_source`](crate::parser::parse_source)) and broadcasts the resulting
//! patch [`EventEnvelope`]s — so editing a file updates the live graph a connected
//! client renders (the Phase-0 headline behaviour, `BUILD_PLAN.md` Phase 0).
//!
//! Alongside the watcher pump, [`run`] spawns the Phase-5 CLV collector
//! ([`collect`](crate::collector::collect)), which tails
//! `<root>/.lattice/clv.ndjson` and folds each correlated `test`/`status` event
//! onto its node's colour — so a failing test reddens its node within ~1s on the
//! same live graph the watcher feeds. [`RunHandle::shutdown`] aborts the watcher,
//! the pump, the collector, and the optional persistence task.
//!
//! Persistence is an **opt-in, best-effort write-through** (Phase 7,
//! `DATA_MODEL.md` §B): set `LATTICE_DB_URL` and [`run`] also subscribes a task that
//! durably records the structured event stream to that database via
//! [`crate::storage`]; unset, the backend runs exactly as before. With a database set,
//! [`run_with_db_url`] additionally **crash-rebuilds** (Phase 9): before serving, it
//! warm-starts the in-memory [`Graph`] from the persisted run-session records
//! ([`Storage::load_nodes`](crate::storage::Storage::load_nodes) /
//! [`load_edges`](crate::storage::Storage::load_edges) → [`Graph::from_records`]) so a
//! restart's first snapshot reflects the prior run, then the filesystem re-parse
//! reconciles drift. The store is observability rather than a hard dependency, so a
//! storage failure (bad URL, unreachable DB, schema error, rehydrate read error,
//! per-event write error) is logged and **degrades gracefully** to the
//! empty-then-parse path — the watch/WS/collector path is never failed or interrupted
//! by it. Tests drive [`run_with_db_url`] to pass an explicit URL without the
//! process-global env var.
//!
//! File paths are normalised to repo-relative form (matching `DATA_MODEL.md` §A.1
//! ids such as `fn:a.rs:alpha`) by stripping the **canonicalised** repository
//! root; this also neutralises the macOS `/var` → `/private/var` tempdir symlink
//! so node ids never leak an absolute path.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;
use walkdir::WalkDir;

use crate::collector::collect;
use crate::graph::Graph;
use crate::parser::parse_source;
use crate::storage::open_store;
use crate::watcher::{is_source_file, watch};
use crate::wire::EventEnvelope;
use crate::ws::{serve, BoundServer};

/// Capacity of the broadcast channel fanning patch events out to WS clients.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Session id recorded for a single local run's `sessions` row when persistence is
/// enabled (`DATA_MODEL.md` §B.6). Matches the [`Graph`] default session id
/// (`"sess-local"`) so the run's `sessions` row aligns with the in-memory graph.
const RUN_SESSION_ID: &str = "sess-local";

/// A running Lattice backend: the WebSocket server plus the watcher pump.
///
/// Holds the bound server [`addr`](RunHandle::addr) (read it to connect) and the
/// background tasks. [`RunHandle::shutdown`] stops the server and the watcher.
/// `store_task` is the optional opt-in persistence task: it is [`Some`] only when a
/// `LATTICE_DB_URL` was provided **and** the store opened and applied its schema
/// successfully ([`run_with_db_url`]); it is [`None`] when persistence is disabled
/// or storage failed to initialise (graceful degradation).
pub struct RunHandle {
    /// The address the WebSocket server is listening on.
    pub addr: SocketAddr,
    server: BoundServer,
    watcher_task: JoinHandle<()>,
    pump_task: JoinHandle<()>,
    collector_task: JoinHandle<()>,
    store_task: Option<JoinHandle<()>>,
}

impl RunHandle {
    /// Stops the WebSocket server, the watcher pump, the CLV collector, and (when
    /// present) the persistence task, and waits for teardown.
    pub async fn shutdown(self) {
        self.server.shutdown().await;
        self.watcher_task.abort();
        self.pump_task.abort();
        self.collector_task.abort();
        if let Some(store_task) = self.store_task {
            store_task.abort();
        }
    }
}

/// Normalises `path` to a repo-relative, forward-slashed string under `root`.
///
/// `root` is assumed already canonicalised; `path` is canonicalised here so the
/// macOS tempdir symlink does not leak an absolute path into node ids. Returns
/// `None` when the path is outside `root` or cannot be canonicalised (e.g. it was
/// just deleted).
fn repo_relative(root: &Path, path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let rel = canonical.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Reads, parses, and applies one changed source file into `graph`, returning patch events.
///
/// The repo-relative path is lowered through [`parse_source`], which dispatches on
/// the file extension (Rust/Python/TypeScript, else a bare `file` node). Returns an
/// empty vector (logging the cause) when the file is outside `root` or cannot be
/// read; never panics.
async fn ingest_file(graph: &Arc<Mutex<Graph>>, root: &Path, path: &Path) -> Vec<EventEnvelope> {
    let Some(rel) = repo_relative(root, path) else {
        return Vec::new();
    };
    let source = match tokio::fs::read_to_string(path).await {
        Ok(source) => source,
        Err(error) => {
            eprintln!("lattice: cannot read {}: {error}", path.display());
            return Vec::new();
        }
    };
    let parsed = parse_source(&rel, &source);
    graph.lock().await.apply_parsed(parsed)
}

/// Default WebSocket listen address used when `LATTICE_ADDR` is unset.
///
/// A fixed default (rather than an ephemeral `:0`) so the bundled frontend can
/// connect to a known port without discovery.
pub const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:7000";

/// Resolves the server's listen address from an optional `LATTICE_ADDR` override.
///
/// `raw` is the env value (`None` when unset → [`DEFAULT_LISTEN_ADDR`]); a present
/// value is parsed as a [`SocketAddr`]. On a parse failure the offending text is
/// returned in the `Err` so the binary can print a clear startup error.
pub fn resolve_listen_addr(raw: Option<&str>) -> Result<SocketAddr, String> {
    let value = raw.unwrap_or(DEFAULT_LISTEN_ADDR);
    value
        .parse()
        .map_err(|error| format!("invalid listen address '{value}': {error}"))
}

/// Starts the Lattice backend against `root`, listening on `addr`.
///
/// Thin wrapper over [`run_with_db_url`] that reads the persistence target from the
/// `LATTICE_DB_URL` environment variable: when it is **set**, the run additionally
/// durably records the structured CLV event stream to that database (opt-in and
/// best-effort — see [`run_with_db_url`]); when it is **unset**, the backend behaves
/// exactly as before, with no database. Pass `127.0.0.1:0` for an ephemeral port and
/// read [`RunHandle::addr`] back.
///
/// # Errors
/// Returns an [`std::io::Error`] if `root` cannot be canonicalised or the server
/// cannot bind to `addr`. A storage problem never fails the run — it degrades to no
/// persistence (see [`run_with_db_url`]).
pub async fn run(root: PathBuf, addr: SocketAddr) -> std::io::Result<RunHandle> {
    run_with_db_url(root, addr, std::env::var("LATTICE_DB_URL").ok()).await
}

/// Starts the Lattice backend against `root` on `addr`, optionally crash-rebuilding
/// the graph from `db_url` and write-through persisting the structured event stream
/// back to it.
///
/// Boot order is **rehydrate, then reconcile, then serve** (Phase 9, crash-rebuild):
/// 1. Canonicalise `root` and, when `db_url` is [`Some`], [`open_store`] +
///    [`ensure_schema`](crate::storage::Storage::ensure_schema) +
///    [`record_session`](crate::storage::Storage::record_session) (keyed by
///    [`RUN_SESSION_ID`]).
/// 2. **Warm start:** while the store is still owned (before the persist task moves
///    it), read the persisted run-session graph via
///    [`load_nodes`](crate::storage::Storage::load_nodes) /
///    [`load_edges`](crate::storage::Storage::load_edges) — keyed on the
///    [`RUN_SESSION_ID`] constant, **not** a most-recent-session lookup — and rebuild
///    the initial [`Graph`] with [`Graph::from_records`]. With no `db_url` (or on any
///    read failure) the initial graph is an empty [`Graph::new`].
/// 3. **Reconcile:** the WalkDir initial parse re-parses every on-disk source file
///    (Rust, Python, or TypeScript) over the warm-started graph — the **filesystem
///    wins**, so a stale persisted file is corrected and an empty graph is filled.
/// 4. Start the WebSocket [`serve`]r (whose first snapshot now reflects the
///    rehydrated-and-reconciled roots), then spawn the watcher pump (re-parses
///    changed files, broadcasting patch events) and the CLV [`collect`]or.
///
/// When `db_url` is [`Some`], the **best-effort** persistence task subscribes to the
/// broadcast channel and write-throughs every structured [`EventEnvelope`] via
/// [`persist`](crate::storage::Storage::persist). Only structured events flow on that
/// channel, so raw/untagged stdout is never persisted (`DATA_MODEL.md` §B.5). When
/// `db_url` is [`None`] the backend runs with no database, identical to before the
/// crash-rebuild story (empty graph, then filesystem parse).
///
/// # Graceful degradation
/// Storage is observability, not a hard dependency, so it must never take down the
/// watch/WS/collector path. If [`open_store`],
/// [`ensure_schema`](crate::storage::Storage::ensure_schema), or a rehydrate read
/// ([`load_nodes`](crate::storage::Storage::load_nodes) /
/// [`load_edges`](crate::storage::Storage::load_edges)) fails, the failure is logged
/// to stderr and the run continues on the **empty-then-parse** path (no warm start;
/// [`RunHandle::store_task`](RunHandle) is [`None`] for an open/schema failure). A
/// per-event [`persist`](crate::storage::Storage::persist) error is likewise logged
/// and skipped — the persistence task never panics and keeps consuming the channel.
///
/// # Errors
/// Returns an [`std::io::Error`] if `root` cannot be canonicalised or the server
/// cannot bind to `addr`. A storage problem is *not* an error here — it degrades to
/// no persistence as described above.
pub async fn run_with_db_url(
    root: PathBuf,
    addr: SocketAddr,
    db_url: Option<String>,
) -> std::io::Result<RunHandle> {
    let root = std::fs::canonicalize(&root)?;
    let (events_tx, _) = broadcast::channel::<EventEnvelope>(EVENT_CHANNEL_CAPACITY);

    // Opt-in, best-effort write-through persistence with a crash-rebuild warm start.
    // The store is opened and **read for rehydration here** — while it is still owned,
    // BEFORE the persist task moves it (`store` is a `Box<dyn Storage>`, not shared) —
    // so the initial graph can be rebuilt from the DB (`load_nodes`/`load_edges` keyed
    // on the `RUN_SESSION_ID` constant, not a most-recent-session lookup) ahead of the
    // WalkDir re-parse that reconciles drift. The subscriber is registered here —
    // before any task can send — so no structured event is missed. Every storage/read
    // failure logs and degrades to an empty `Graph::new()` (then-parse), exactly like
    // the no-`db_url` path; persistence is observability, never a hard dependency.
    let (initial_graph, store_task) = match db_url {
        None => (Graph::new(), None),
        Some(url) => match open_store(&url).await {
            Ok(store) => {
                if let Err(error) = store.ensure_schema().await {
                    eprintln!("lattice: storage disabled (schema error: {error})");
                    (Graph::new(), None)
                } else {
                    if let Err(error) = store
                        .record_session(RUN_SESSION_ID, &root.display().to_string())
                        .await
                    {
                        // Best-effort: log but keep persisting — `persist` lazily
                        // upserts the session row on first sight (`DATA_MODEL.md` §B.2).
                        eprintln!("lattice: record_session failed: {error}");
                    }
                    // Crash-rebuild warm start: read the persisted run-session graph
                    // BEFORE the store is moved into the persist task. A read error
                    // (e.g. a corrupted row) degrades to an empty graph (then-parse).
                    let graph = match (
                        store.load_nodes(RUN_SESSION_ID).await,
                        store.load_edges(RUN_SESSION_ID).await,
                    ) {
                        (Ok(nodes), Ok(edges)) => Graph::from_records(RUN_SESSION_ID, nodes, edges),
                        (Err(error), _) | (_, Err(error)) => {
                            eprintln!("lattice: rehydrate disabled (read error: {error})");
                            Graph::new()
                        }
                    };
                    let mut rx = events_tx.subscribe();
                    let task = tokio::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(env) => {
                                    if let Err(error) = store.persist(&env).await {
                                        eprintln!("lattice: persist error: {error}");
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                                    // Best-effort persistence: the store fell behind the
                                    // channel capacity, so `dropped` events are gone from the DB
                                    // (the in-memory Graph stays the source of truth).
                                    eprintln!("lattice: persist lagged, dropped {dropped} events");
                                    continue;
                                }
                                Err(broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    });
                    (graph, Some(task))
                }
            }
            Err(error) => {
                eprintln!("lattice: storage disabled ({error})");
                (Graph::new(), None)
            }
        },
    };

    let graph = Arc::new(Mutex::new(initial_graph));

    // Initial parse: reconcile the (possibly warm-started) graph against the on-disk
    // source — the filesystem wins, so a re-parse corrects any drift in the rehydrated
    // graph and fills an empty one. The first snapshot reflects the repo.
    for entry in WalkDir::new(&root).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if is_source_file(path) {
            let _ = ingest_file(&graph, &root, path).await;
        }
    }

    let server = serve(addr, Arc::clone(&graph), events_tx.clone()).await?;
    let addr = server.addr;

    // Watcher pump: re-parse changed files and broadcast their patch events.
    let (watch_tx, mut watch_rx) = mpsc::channel::<PathBuf>(64);
    let watcher_task = tokio::spawn(watch(root.clone(), watch_tx));
    let pump_graph = Arc::clone(&graph);
    let pump_root = root.clone();
    let pump_events = events_tx.clone();
    let pump_task = tokio::spawn(async move {
        while let Some(path) = watch_rx.recv().await {
            for event in ingest_file(&pump_graph, &pump_root, &path).await {
                let _ = pump_events.send(event);
            }
        }
    });

    // CLV collector: tail `<root>/.lattice/clv.ndjson` and fold each correlated
    // test/status event onto its node's colour, broadcasting the patch envelope.
    let collector_task = tokio::spawn(collect(root.clone(), Arc::clone(&graph), events_tx.clone()));

    Ok(RunHandle {
        addr,
        server,
        watcher_task,
        pump_task,
        collector_task,
        store_task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher::DEBOUNCE;
    use crate::wire::{Edge, EventType, Node, NodeStatus, Payload, TestOutcome};
    use futures_util::{SinkExt, StreamExt};
    use std::io::Write;
    use std::path::Path;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::time::timeout;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    type ClientWs = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    fn local() -> SocketAddr {
        "127.0.0.1:0".parse().expect("valid loopback addr")
    }

    #[test]
    fn resolve_listen_addr_defaults_parses_and_errors() {
        assert_eq!(
            resolve_listen_addr(None).expect("default parses"),
            DEFAULT_LISTEN_ADDR.parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            resolve_listen_addr(Some("127.0.0.1:9999"))
                .expect("override parses")
                .port(),
            9999
        );
        assert!(resolve_listen_addr(Some("not-an-addr")).is_err());
    }

    /// Reads frames until one parses as an [`EventEnvelope`], or times out.
    async fn next_envelope(ws: &mut ClientWs) -> EventEnvelope {
        loop {
            let frame = timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("a frame within budget")
                .expect("stream still open")
                .expect("a non-error frame");
            if let Message::Text(text) = frame {
                if let Ok(env) = serde_json::from_str::<EventEnvelope>(text.as_str()) {
                    return env;
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initial_snapshot_is_root_only_then_expand_yields_the_function() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn alpha() {}").expect("write");
        let handle = run(dir.path().to_path_buf(), local()).await.expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");

        // The initial snapshot is lazy: the root file node, but not its function child.
        let env = next_envelope(&mut ws).await;
        assert_eq!(env.event_type, EventType::Snapshot);
        match env.payload {
            Payload::Snapshot { nodes, .. } => {
                assert!(
                    nodes.iter().any(|n| n.id == "file:a.rs"),
                    "snapshot must carry the root file node: {:?}",
                    nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
                );
                assert!(
                    !nodes.iter().any(|n| n.id == "fn:a.rs:alpha"),
                    "lazy snapshot must NOT carry the child function: {:?}",
                    nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
                );
            }
            other => panic!("expected snapshot, got {other:?}"),
        }

        // Expanding the repo-relative file id yields a subtree with the function.
        ws.send(Message::text(
            "{\"type\":\"expand\",\"nodeId\":\"file:a.rs\"}",
        ))
        .await
        .expect("send expand");
        let subtree = next_envelope(&mut ws).await;
        assert_eq!(subtree.event_type, EventType::Subtree);
        match subtree.payload {
            Payload::Subtree {
                parent_id, nodes, ..
            } => {
                assert_eq!(parent_id, "file:a.rs");
                assert!(
                    nodes.iter().any(|n| n.id == "fn:a.rs:alpha"),
                    "expand must reveal the function child: {:?}",
                    nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
                );
            }
            other => panic!("expected subtree, got {other:?}"),
        }
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn python_initial_snapshot_is_root_only_then_expand_yields_the_function() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("m.py"), "def alpha():\n    pass\n").expect("write");
        let handle = run(dir.path().to_path_buf(), local()).await.expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");

        // Lazy snapshot: the Python file node, but not its function child.
        let env = next_envelope(&mut ws).await;
        assert_eq!(env.event_type, EventType::Snapshot);
        match env.payload {
            Payload::Snapshot { nodes, .. } => {
                assert!(
                    nodes.iter().any(|n| n.id == "file:m.py"),
                    "snapshot must carry the Python file node: {:?}",
                    nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
                );
                assert!(
                    !nodes.iter().any(|n| n.id == "fn:m.py:alpha"),
                    "lazy snapshot must NOT carry the child function: {:?}",
                    nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
                );
            }
            other => panic!("expected snapshot, got {other:?}"),
        }

        ws.send(Message::text(
            "{\"type\":\"expand\",\"nodeId\":\"file:m.py\"}",
        ))
        .await
        .expect("send expand");
        let subtree = next_envelope(&mut ws).await;
        assert_eq!(subtree.event_type, EventType::Subtree);
        match subtree.payload {
            Payload::Subtree {
                parent_id, nodes, ..
            } => {
                assert_eq!(parent_id, "file:m.py");
                assert!(
                    nodes.iter().any(|n| n.id == "fn:m.py:alpha"),
                    "expand must reveal the Python function child: {:?}",
                    nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
                );
            }
            other => panic!("expected subtree, got {other:?}"),
        }
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typescript_initial_snapshot_is_root_only_then_expand_yields_the_function() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("t.ts"), "function alpha() {}").expect("write");
        let handle = run(dir.path().to_path_buf(), local()).await.expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");

        // Lazy snapshot: the TypeScript file node, but not its function child.
        let env = next_envelope(&mut ws).await;
        assert_eq!(env.event_type, EventType::Snapshot);
        match env.payload {
            Payload::Snapshot { nodes, .. } => {
                assert!(
                    nodes.iter().any(|n| n.id == "file:t.ts"),
                    "snapshot must carry the TypeScript file node: {:?}",
                    nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
                );
                assert!(
                    !nodes.iter().any(|n| n.id == "fn:t.ts:alpha"),
                    "lazy snapshot must NOT carry the child function: {:?}",
                    nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
                );
            }
            other => panic!("expected snapshot, got {other:?}"),
        }

        ws.send(Message::text(
            "{\"type\":\"expand\",\"nodeId\":\"file:t.ts\"}",
        ))
        .await
        .expect("send expand");
        let subtree = next_envelope(&mut ws).await;
        assert_eq!(subtree.event_type, EventType::Subtree);
        match subtree.payload {
            Payload::Subtree {
                parent_id, nodes, ..
            } => {
                assert_eq!(parent_id, "file:t.ts");
                assert!(
                    nodes.iter().any(|n| n.id == "fn:t.ts:alpha"),
                    "expand must reveal the TypeScript function child: {:?}",
                    nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
                );
            }
            other => panic!("expected subtree, got {other:?}"),
        }
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn renaming_a_function_emits_upsert_beta_and_remove_alpha() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn alpha() {}").expect("write");
        let handle = run(dir.path().to_path_buf(), local()).await.expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let _snapshot = next_envelope(&mut ws).await; // drain the initial snapshot
        tokio::time::sleep(Duration::from_millis(200)).await; // let the watcher settle
        std::fs::write(&file, "fn beta() {}").expect("rewrite");

        let mut saw_upsert_beta = false;
        let mut saw_remove_alpha = false;
        let outcome = timeout(DEBOUNCE + Duration::from_secs(4), async {
            while !(saw_upsert_beta && saw_remove_alpha) {
                let env = next_envelope(&mut ws).await;
                match (env.event_type, env.payload) {
                    (EventType::NodeUpsert, Payload::NodeUpsert { node })
                        if node.id == "fn:a.rs:beta" =>
                    {
                        saw_upsert_beta = true
                    }
                    (EventType::NodeRemove, Payload::NodeRemove { id })
                        if id == "fn:a.rs:alpha" =>
                    {
                        saw_remove_alpha = true
                    }
                    _ => {}
                }
            }
        })
        .await;
        assert!(
            outcome.is_ok(),
            "missing events: upsert(beta)={saw_upsert_beta} remove(alpha)={saw_remove_alpha}"
        );
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn broken_file_node_is_error_and_server_still_serves() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("bad.rs"), "fn bad( {").expect("write");
        let handle = run(dir.path().to_path_buf(), local()).await.expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let env = next_envelope(&mut ws).await;
        match env.payload {
            Payload::Snapshot { nodes, .. } => {
                let file_node = nodes
                    .iter()
                    .find(|n| n.id == "file:bad.rs")
                    .expect("broken file node present");
                assert_eq!(file_node.status, NodeStatus::Error);
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
        handle.shutdown().await;
    }

    /// Appends one CLV line (terminated with `\n`) to `<root>/.lattice/clv.ndjson`,
    /// creating the `.lattice` directory and sink file if absent — mirroring an
    /// external emitter (a `PostToolUse` hook or test reporter) writing the sink.
    fn append_sink_line(root: &Path, line: &str) {
        let dir = root.join(".lattice");
        std::fs::create_dir_all(&dir).expect("create .lattice dir");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("clv.ndjson"))
            .expect("open sink for append");
        writeln!(file, "{line}").expect("append sink line");
    }

    /// Writes raw bytes to the sink with **no** added newline — used to split one
    /// logical line across two writes for the partial-line test.
    fn append_sink_raw(root: &Path, bytes: &str) {
        let dir = root.join(".lattice");
        std::fs::create_dir_all(&dir).expect("create .lattice dir");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("clv.ndjson"))
            .expect("open sink for append");
        file.write_all(bytes.as_bytes()).expect("append sink bytes");
    }

    /// Reads envelopes until a `test.result` for `node_id` arrives (or times out).
    async fn next_test_result_for(ws: &mut ClientWs, node_id: &str) -> EventEnvelope {
        loop {
            let env = next_envelope(ws).await;
            if env.event_type == EventType::TestResult {
                if let Payload::TestResult { node_id: nid, .. } = &env.payload {
                    if nid == node_id {
                        return env;
                    }
                }
            }
        }
    }

    /// Sends an `expand` for `file_id` and returns the subtree's child nodes,
    /// skipping any patch envelopes that arrive before the subtree reply.
    async fn expand_subtree_nodes(ws: &mut ClientWs, file_id: &str) -> Vec<Node> {
        ws.send(Message::text(format!(
            "{{\"type\":\"expand\",\"nodeId\":\"{file_id}\"}}"
        )))
        .await
        .expect("send expand");
        loop {
            let env = next_envelope(ws).await;
            if let Payload::Subtree {
                parent_id, nodes, ..
            } = env.payload
            {
                if parent_id == file_id {
                    return nodes;
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn appending_a_failing_test_line_recolours_the_node() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn f() {}").expect("write");
        let handle = run(dir.path().to_path_buf(), local()).await.expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let _snapshot = next_envelope(&mut ws).await; // drain the root snapshot

        append_sink_line(
            dir.path(),
            r#"#CLV1 {"event":"test","session":"s1","pid":1,"node":"fn:a.rs:f","outcome":"fail"}"#,
        );

        // The collector delivers a test.result for fn:a.rs:f within ~1-2s.
        let env = next_test_result_for(&mut ws, "fn:a.rs:f").await;
        assert_eq!(env.event_type, EventType::TestResult);

        // A fresh subtree reflects the stored Failing colour.
        let nodes = expand_subtree_nodes(&mut ws, "file:a.rs").await;
        let f = nodes
            .iter()
            .find(|n| n.id == "fn:a.rs:f")
            .expect("function child present");
        assert_eq!(f.status, NodeStatus::Failing);
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sink_absent_at_startup_then_created_still_delivers() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn f() {}").expect("write");
        // Deliberately do NOT create `.lattice` — the collector must tolerate it.
        let handle = run(dir.path().to_path_buf(), local()).await.expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let _snapshot = next_envelope(&mut ws).await;

        // Creating the sink and appending later still delivers the event.
        append_sink_line(
            dir.path(),
            r#"#CLV1 {"event":"test","session":"s1","node":"fn:a.rs:f","outcome":"fail"}"#,
        );
        let env = next_test_result_for(&mut ws, "fn:a.rs:f").await;
        assert_eq!(env.event_type, EventType::TestResult);
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_line_is_parsed_only_after_its_newline() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn f() {}").expect("write");
        let handle = run(dir.path().to_path_buf(), local()).await.expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let _snapshot = next_envelope(&mut ws).await;

        // Write the line without its trailing newline: it must NOT parse yet.
        append_sink_raw(
            dir.path(),
            r#"#CLV1 {"event":"test","session":"s1","node":"fn:a.rs:f","outcome":"fail"#,
        );
        // Several poll cycles pass with no envelope (the buffered partial waits).
        assert!(
            timeout(Duration::from_millis(900), ws.next())
                .await
                .is_err(),
            "no envelope must arrive before the newline closes the line"
        );

        // Completing the line delivers the event exactly once.
        append_sink_raw(dir.path(), "\"}\n");
        let env = next_test_result_for(&mut ws, "fn:a.rs:f").await;
        assert_eq!(env.event_type, EventType::TestResult);
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_lines_are_skipped_and_tailing_continues() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn f() {}").expect("write");
        let handle = run(dir.path().to_path_buf(), local()).await.expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let _snapshot = next_envelope(&mut ws).await;

        // An untagged line and a malformed `#CLV1` line: both produce no envelope.
        append_sink_line(dir.path(), "PASS app/foo.test.ts");
        append_sink_line(dir.path(), "#CLV1 {");
        // A subsequent valid line is still delivered — the tailer did not stop.
        append_sink_line(
            dir.path(),
            r#"#CLV1 {"event":"test","session":"s1","node":"fn:a.rs:f","outcome":"fail"}"#,
        );

        let env = next_test_result_for(&mut ws, "fn:a.rs:f").await;
        assert_eq!(env.event_type, EventType::TestResult);
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_sessions_colour_their_own_nodes_independently() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn f() {}\nfn g() {}").expect("write");
        let handle = run(dir.path().to_path_buf(), local()).await.expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let _snapshot = next_envelope(&mut ws).await;

        // Interleaved sessions writing the same sink, each to its own node id.
        append_sink_line(
            dir.path(),
            r#"#CLV1 {"event":"test","session":"s1","node":"fn:a.rs:f","outcome":"fail"}"#,
        );
        append_sink_line(
            dir.path(),
            r#"#CLV1 {"event":"test","session":"s2","node":"fn:a.rs:g","outcome":"pass"}"#,
        );

        // Both events are delivered, each carrying its own node id and outcome.
        let mut saw_f = false;
        let mut saw_g = false;
        while !(saw_f && saw_g) {
            let env = next_envelope(&mut ws).await;
            if let Payload::TestResult {
                node_id, outcome, ..
            } = &env.payload
            {
                if node_id == "fn:a.rs:f" && *outcome == TestOutcome::Fail {
                    saw_f = true;
                }
                if node_id == "fn:a.rs:g" && *outcome == TestOutcome::Pass {
                    saw_g = true;
                }
            }
        }

        // A fresh subtree colours f Failing and g Passing — no cross-contamination.
        let nodes = expand_subtree_nodes(&mut ws, "file:a.rs").await;
        let f = nodes
            .iter()
            .find(|n| n.id == "fn:a.rs:f")
            .expect("f present");
        let g = nodes
            .iter()
            .find(|n| n.id == "fn:a.rs:g")
            .expect("g present");
        assert_eq!(f.status, NodeStatus::Failing);
        assert_eq!(g.status, NodeStatus::Passing);
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_tears_down_collector_without_hanging() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn f() {}").expect("write");
        let handle = run(dir.path().to_path_buf(), local()).await.expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let _snapshot = next_envelope(&mut ws).await;

        // Shutdown must abort the collector promptly — it never hangs on the poll
        // loop, proving the collector task is aborted (not leaked).
        timeout(Duration::from_secs(2), handle.shutdown())
            .await
            .expect("shutdown completes promptly, aborting the collector");
    }

    /// Polls the sqlite DB at `db_url` up to ~2s for the count of `test_results`
    /// rows whose `node_id` equals `node`, returning the count once non-zero (or 0
    /// after the budget). Mirrors [`next_test_result_for`]'s wait pattern; tolerates
    /// a not-yet-applied schema by treating a query error as `0` and retrying.
    async fn poll_test_result_count(db_url: &str, node: &str) -> i64 {
        let pool = sqlx::SqlitePool::connect(db_url)
            .await
            .expect("open db for read");
        let mut count = 0i64;
        for _ in 0..40 {
            count = sqlx::query_scalar("SELECT COUNT(*) FROM test_results WHERE node_id = ?")
                .bind(node)
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
            if count > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        pool.close().await;
        count
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_url_set_persists_the_structured_test_event() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("x.rs"), "fn f() {}").expect("write");
        let db_dir = tempdir().expect("db tempdir");
        let db_url = format!("sqlite://{}", db_dir.path().join("events.db").display());
        let handle = run_with_db_url(dir.path().to_path_buf(), local(), Some(db_url.clone()))
            .await
            .expect("run");

        // A structured CLV test line for an in-graph node yields a TestResult
        // envelope, which the persistence task write-throughs to `test_results`.
        append_sink_line(
            dir.path(),
            r#"#CLV1 {"event":"test","session":"s1","pid":1,"node":"fn:x.rs:f","outcome":"fail"}"#,
        );

        let count = poll_test_result_count(&db_url, "fn:x.rs:f").await;
        assert_eq!(
            count, 1,
            "the structured test event must persist exactly one test_results row"
        );
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_url_none_runs_with_no_persistence() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("x.rs"), "fn f() {}").expect("write");
        let handle = run_with_db_url(dir.path().to_path_buf(), local(), None)
            .await
            .expect("run");
        assert!(
            handle.store_task.is_none(),
            "no persistence task when LATTICE_DB_URL is unset"
        );

        // Serves exactly as today: a root snapshot still arrives.
        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let env = next_envelope(&mut ws).await;
        assert_eq!(env.event_type, EventType::Snapshot);
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_aborts_the_store_task_without_hanging() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("x.rs"), "fn f() {}").expect("write");
        let db_dir = tempdir().expect("db tempdir");
        let db_url = format!("sqlite://{}", db_dir.path().join("events.db").display());
        let handle = run_with_db_url(dir.path().to_path_buf(), local(), Some(db_url))
            .await
            .expect("run");
        assert!(
            handle.store_task.is_some(),
            "a persistence task exists when LATTICE_DB_URL is set"
        );
        // The store task is an endless recv loop; a prompt shutdown proves it is
        // aborted (not leaked) rather than hanging the teardown.
        timeout(Duration::from_secs(2), handle.shutdown())
            .await
            .expect("shutdown aborts the store task without hanging");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn raw_and_malformed_sink_lines_are_never_persisted() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("x.rs"), "fn f() {}").expect("write");
        let db_dir = tempdir().expect("db tempdir");
        let db_url = format!("sqlite://{}", db_dir.path().join("events.db").display());
        let handle = run_with_db_url(dir.path().to_path_buf(), local(), Some(db_url.clone()))
            .await
            .expect("run");

        // Neither an untagged stdout line nor a malformed `#CLV1` line parses to an
        // envelope, so neither reaches the store.
        append_sink_line(dir.path(), "PASS foo");
        append_sink_line(dir.path(), "#CLV1 {garbage");
        // A trailing structured line DOES persist — the barrier proving the collector
        // has processed (and skipped) the two non-persisting lines above it in order.
        append_sink_line(
            dir.path(),
            r#"#CLV1 {"event":"test","session":"s1","node":"fn:x.rs:f","outcome":"fail"}"#,
        );

        // Wait for the structured line to land, then assert it is the ONLY row.
        let by_node = poll_test_result_count(&db_url, "fn:x.rs:f").await;
        assert_eq!(by_node, 1, "the structured line persists");
        let pool = sqlx::SqlitePool::connect(&db_url)
            .await
            .expect("open db for read");
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM test_results")
            .fetch_one(&pool)
            .await
            .expect("count all");
        pool.close().await;
        assert_eq!(
            total, 1,
            "raw and malformed sink lines must not persist any test_results row"
        );
        handle.shutdown().await;
    }

    // ---- P9-2: crash-rebuild — rehydrate the graph from the DB at startup ----
    //
    // These integration tests mirror the `initial_snapshot_is_root_only_…` full-stack
    // template: seed a file-backed SQLite DB with a prior run's persisted graph, then
    // boot `run_with_db_url` against it and assert the FIRST `snapshot` already
    // reflects the persisted roots — i.e. the warm start rehydrated the graph BEFORE
    // any file was re-parsed (`Storage::load_nodes`/`load_edges` →
    // `Graph::from_records`, keyed on the `RUN_SESSION_ID` constant). Tests 1, 2 and 5
    // are RED until that wiring exists (the graph boots empty today); tests 3 and 4
    // guard the no-DB and read-error degradation paths.

    /// Builds a `node.upsert` [`EventEnvelope`] stamped with `session_id`, modelling a
    /// prior run's write-through of one parsed node (the sqlite `node.upsert` arm keys
    /// the persisted `nodes` row on `env.session_id`). Test-only.
    fn upsert_node_env(session_id: &str, node: Node) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-01-01T00:00:00Z".to_string(),
            session_id: session_id.to_string(),
            event_type: EventType::NodeUpsert,
            payload: Payload::NodeUpsert { node },
        }
    }

    /// Builds an `edge.upsert` [`EventEnvelope`] stamped with `session_id` — the edge
    /// twin of [`upsert_node_env`]. Test-only.
    fn upsert_edge_env(session_id: &str, edge: Edge) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-01-01T00:00:00Z".to_string(),
            session_id: session_id.to_string(),
            event_type: EventType::EdgeUpsert,
            payload: Payload::EdgeUpsert { edge },
        }
    }

    /// Builds a `test.result` [`EventEnvelope`] under `session_id` — used to plant a
    /// distinct, later-started `sessions` row so the multi-session regression can prove
    /// rehydrate keys on [`RUN_SESSION_ID`], not the most-recent session. Test-only.
    fn test_result_env(session_id: &str, node_id: &str) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-02-02T00:00:00Z".to_string(),
            session_id: session_id.to_string(),
            event_type: EventType::TestResult,
            payload: Payload::TestResult {
                node_id: node_id.to_string(),
                test_id: "t-seed".to_string(),
                outcome: TestOutcome::Pass,
                duration_ms: None,
                session_id: session_id.to_string(),
                agent_id: None,
                process_id: None,
                message: None,
            },
        }
    }

    /// Seeds a file-backed SQLite DB at `db_url` with the parsed nodes/edges of each
    /// `(path, src)` file, persisted as `node.upsert`/`edge.upsert` envelopes under
    /// `session_id` — modelling exactly what a prior run's write-through persistence
    /// stored for a session's graph (`DATA_MODEL.md` §B).
    ///
    /// Test-only helper. The store's pool is dropped on return; the committed rows
    /// survive on the file-backed database for a later [`open_store`] (the run under
    /// test) to read back via [`Storage::load_nodes`]/[`Storage::load_edges`].
    async fn seed_persisted_graph(db_url: &str, session_id: &str, files: &[(&str, &str)]) {
        let store = open_store(db_url).await.expect("open seed store");
        store.ensure_schema().await.expect("seed schema");
        store
            .record_session(session_id, "/seed/repo")
            .await
            .expect("seed sessions row");
        for (path, src) in files {
            let parsed = parse_source(path, src);
            for node in parsed.nodes {
                store
                    .persist(&upsert_node_env(session_id, node))
                    .await
                    .expect("persist seed node");
            }
            for edge in parsed.edges {
                store
                    .persist(&upsert_edge_env(session_id, edge))
                    .await
                    .expect("persist seed edge");
            }
        }
    }

    /// Collects a `snapshot` envelope's root node ids (panicking if the first envelope
    /// is not a snapshot) — the first frame the server sends on connect (`ws.rs:135`).
    fn snapshot_root_ids(env: &EventEnvelope) -> Vec<String> {
        match &env.payload {
            Payload::Snapshot { nodes, .. } => nodes.iter().map(|n| n.id.clone()).collect(),
            other => panic!("expected a snapshot envelope, got {other:?}"),
        }
    }

    /// AC#1 (rebuild-from-seeded-DB). A file-backed SQLite DB pre-seeded with persisted
    /// nodes/edges for [`RUN_SESSION_ID`], booted against an **empty** repo dir (so the
    /// WalkDir re-parse adds nothing), must yield a **non-empty** first `snapshot`
    /// carrying the persisted roots — proving the graph was rehydrated from the DB
    /// BEFORE any file was re-parsed.
    ///
    /// RED today: `run_with_db_url` boots `Graph::new()` (always empty) and the empty
    /// repo parses nothing, so the first snapshot has no `file:` roots.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rehydrate_from_seeded_db_first_snapshot_carries_persisted_roots() {
        let db_dir = tempdir().expect("db tempdir");
        let db_url = format!("sqlite://{}", db_dir.path().join("seed.db").display());
        seed_persisted_graph(
            &db_url,
            RUN_SESSION_ID,
            &[
                ("seed_a.rs", "fn alpha() {}"),
                ("seed_b.rs", "fn gamma() {}"),
            ],
        )
        .await;

        // Empty repo dir → the initial WalkDir re-parse adds nothing, so any snapshot
        // content can only have come from the DB rehydrate.
        let repo = tempdir().expect("repo tempdir");
        let handle = run_with_db_url(repo.path().to_path_buf(), local(), Some(db_url))
            .await
            .expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let env = next_envelope(&mut ws).await;
        assert_eq!(env.event_type, EventType::Snapshot);
        let roots = snapshot_root_ids(&env);
        assert!(
            roots.iter().any(|id| id == "file:seed_a.rs"),
            "first snapshot must carry the rehydrated root file:seed_a.rs before any reparse: {roots:?}"
        );
        assert!(
            roots.iter().any(|id| id == "file:seed_b.rs"),
            "first snapshot must carry the rehydrated root file:seed_b.rs before any reparse: {roots:?}"
        );
        // Lazy snapshot: the rehydrated child function is NOT in the root payload.
        assert!(
            !roots.iter().any(|id| id == "fn:seed_a.rs:alpha"),
            "rehydrated snapshot stays lazy — no child function at the root: {roots:?}"
        );
        handle.shutdown().await;
    }

    /// AC#2 (multi-session regression — adversarial-review HIGH). With the DB holding
    /// the [`RUN_SESSION_ID`] graph PLUS a distinct, later-started CLV session (its own
    /// nodes and a `test.result` row), the rehydrate must still load the
    /// [`RUN_SESSION_ID`] nodes — proving it keys on the run-session constant, not a
    /// "most-recent session" lookup (which would load the other session's nodes, or
    /// zero run-session rows).
    ///
    /// RED today: no rehydrate runs at all, so the empty-repo snapshot is empty —
    /// `file:run_file.rs` is absent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rehydrate_keys_on_run_session_not_most_recent_session() {
        let db_dir = tempdir().expect("db tempdir");
        let db_url = format!("sqlite://{}", db_dir.path().join("multi.db").display());

        // The run-session graph — what a warm start must restore.
        seed_persisted_graph(
            &db_url,
            RUN_SESSION_ID,
            &[("run_file.rs", "fn run_fn() {}")],
        )
        .await;

        // A DISTINCT, later-started session with its OWN, different nodes …
        let other = "sess-newer-deadbeef";
        seed_persisted_graph(&db_url, other, &[("other_file.rs", "fn other_fn() {}")]).await;
        // … plus a `test.result` row, so the other session is the most-recently-written
        // `sessions` row a naive most-recent lookup would pick.
        let store = open_store(&db_url)
            .await
            .expect("open for other test.result");
        store
            .persist(&test_result_env(other, "fn:other_file.rs:other_fn"))
            .await
            .expect("persist other session test.result");
        drop(store);

        let repo = tempdir().expect("repo tempdir");
        let handle = run_with_db_url(repo.path().to_path_buf(), local(), Some(db_url))
            .await
            .expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let env = next_envelope(&mut ws).await;
        let roots = snapshot_root_ids(&env);
        assert!(
            roots.iter().any(|id| id == "file:run_file.rs"),
            "rehydrate must load the RUN_SESSION_ID graph: {roots:?}"
        );
        assert!(
            !roots.iter().any(|id| id == "file:other_file.rs"),
            "rehydrate must NOT load a different (most-recent) session's nodes: {roots:?}"
        );
        handle.shutdown().await;
    }

    /// AC#3 (no-DB regression guard). With `db_url == None`, boot is byte-for-byte
    /// today's behaviour: an empty graph filled by the filesystem parse, served as a
    /// lazy root-only `snapshot`. Passes today and must keep passing once rehydrate is
    /// wired (the rehydrate path must never touch the no-DB case).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_db_boot_is_root_only_filesystem_snapshot() {
        let repo = tempdir().expect("repo tempdir");
        std::fs::write(repo.path().join("only.rs"), "fn only() {}").expect("write");
        let handle = run_with_db_url(repo.path().to_path_buf(), local(), None)
            .await
            .expect("run");
        assert!(
            handle.store_task.is_none(),
            "no persistence task when there is no db_url"
        );

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let env = next_envelope(&mut ws).await;
        let roots = snapshot_root_ids(&env);
        assert!(
            roots.iter().any(|id| id == "file:only.rs"),
            "no-DB boot still parses and serves the on-disk file root: {roots:?}"
        );
        assert!(
            !roots.iter().any(|id| id == "fn:only.rs:only"),
            "no-DB snapshot stays lazy — no child function at the root: {roots:?}"
        );
        handle.shutdown().await;
    }

    /// AC#4 (read-error degradation). A persisted node whose `type` column is corrupted
    /// to an unknown wire enum makes [`Storage::load_nodes`] return a
    /// [`StorageError`](crate::storage::StorageError) — the rehydrate read error the
    /// boot must catch, log, and degrade past, falling back to the empty-then-parse
    /// path: the on-disk file is still parsed and served, and the run never panics or
    /// aborts. Passes today (rehydrate is unwired); once wired it exercises the
    /// caught-error branch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rehydrate_read_error_degrades_to_empty_then_parse() {
        let db_dir = tempdir().expect("db tempdir");
        let db_url = format!("sqlite://{}", db_dir.path().join("corrupt.db").display());
        seed_persisted_graph(&db_url, RUN_SESSION_ID, &[("ghost.rs", "fn ghost() {}")]).await;

        // Corrupt the persisted node's enum so the read side cannot reconstruct it —
        // load_nodes must surface a StorageError, never panic.
        let pool = sqlx::SqlitePool::connect(&db_url)
            .await
            .expect("open db to corrupt the row");
        sqlx::query("UPDATE nodes SET type = 'not-a-real-type' WHERE session_id = ?")
            .bind(RUN_SESSION_ID)
            .execute(&pool)
            .await
            .expect("corrupt the persisted node type");
        pool.close().await;

        // A live on-disk file the empty-then-parse fallback must still serve.
        let repo = tempdir().expect("repo tempdir");
        std::fs::write(repo.path().join("live.rs"), "fn live() {}").expect("write live");

        let handle = run_with_db_url(repo.path().to_path_buf(), local(), Some(db_url))
            .await
            .expect("run must not panic/abort on a rehydrate read error");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let env = next_envelope(&mut ws).await;
        let roots = snapshot_root_ids(&env);
        assert!(
            roots.iter().any(|id| id == "file:live.rs"),
            "a rehydrate read error must degrade to empty-then-parse, still serving the on-disk file: {roots:?}"
        );
        assert!(
            !roots.iter().any(|id| id == "file:ghost.rs"),
            "an atomic load error must discard the entire persisted graph — the corrupt seeded node must not leak into the snapshot: {roots:?}"
        );
        handle.shutdown().await;
    }

    /// AC#5 (reconcile-corrects-drift). After a warm start whose persisted graph drifts
    /// from the on-disk source, the re-parse must win: the served graph matches the
    /// re-parsed filesystem for files that still exist on disk, while a persisted file
    /// that is absent on disk remains from the warm start.
    ///
    /// The DB holds `drift.rs` → `old_fn` and a `cold.rs` → `cold_fn` that is NOT on
    /// disk; on disk `drift.rs` instead contains `new_fn`. After rehydrate + reparse:
    /// - `file:cold.rs` is present (warm-started; it never gets re-parsed) — this is the
    ///   RED proof: today the graph boots empty so `file:cold.rs` is absent;
    /// - expanding `file:drift.rs` yields `new_fn` and NOT the stale `old_fn` — reconcile
    ///   wins over the stale DB.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rehydrate_then_reparse_reconciles_drift_filesystem_wins() {
        let db_dir = tempdir().expect("db tempdir");
        let db_url = format!("sqlite://{}", db_dir.path().join("drift.db").display());
        seed_persisted_graph(
            &db_url,
            RUN_SESSION_ID,
            &[
                ("drift.rs", "fn old_fn() {}"),
                ("cold.rs", "fn cold_fn() {}"),
            ],
        )
        .await;

        // On disk, drift.rs has drifted to new_fn; cold.rs is deliberately absent.
        let repo = tempdir().expect("repo tempdir");
        std::fs::write(repo.path().join("drift.rs"), "fn new_fn() {}").expect("write drift");

        let handle = run_with_db_url(repo.path().to_path_buf(), local(), Some(db_url))
            .await
            .expect("run");

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let env = next_envelope(&mut ws).await;
        let roots = snapshot_root_ids(&env);
        // RED proof: the warm-started, on-disk-absent file survives the reparse.
        assert!(
            roots.iter().any(|id| id == "file:cold.rs"),
            "warm start must rehydrate the DB-only file (absent on disk) before reparse — \
             missing means rehydrate is unwired: {roots:?}"
        );
        // The re-parsed file is also a root (from the filesystem).
        assert!(
            roots.iter().any(|id| id == "file:drift.rs"),
            "the on-disk file must be a root after reparse: {roots:?}"
        );

        // Reconcile wins: expanding drift.rs shows the filesystem's new_fn, not the
        // stale persisted old_fn.
        let children = expand_subtree_nodes(&mut ws, "file:drift.rs").await;
        let child_ids: Vec<&str> = children.iter().map(|n| n.id.as_str()).collect();
        assert!(
            child_ids.contains(&"fn:drift.rs:new_fn"),
            "reparse must add the on-disk function fn:drift.rs:new_fn: {child_ids:?}"
        );
        assert!(
            !child_ids.contains(&"fn:drift.rs:old_fn"),
            "reparse must reconcile away the stale persisted fn:drift.rs:old_fn: {child_ids:?}"
        );
        handle.shutdown().await;
    }

    /// AC#4 sibling (open-store fail-soft). A `db_url` whose scheme `open_store` rejects
    /// (`StorageError::Config`) must degrade exactly like the no-DB path: no persistence
    /// task, an empty-then-parse graph, and the on-disk file still served — the run never
    /// fails or panics on a bad storage URL (mirrors the `record_session`/read-error
    /// degradation the story mandates for every storage failure).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsupported_db_url_degrades_to_empty_then_parse() {
        let repo = tempdir().expect("repo tempdir");
        std::fs::write(repo.path().join("kept.rs"), "fn kept() {}").expect("write");

        let handle = run_with_db_url(
            repo.path().to_path_buf(),
            local(),
            Some("mysql://unsupported/scheme".to_string()),
        )
        .await
        .expect("run must not fail on an unsupported db_url");
        assert!(
            handle.store_task.is_none(),
            "an unsupported db_url scheme must disable persistence (no store task)"
        );

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let env = next_envelope(&mut ws).await;
        let roots = snapshot_root_ids(&env);
        assert!(
            roots.iter().any(|id| id == "file:kept.rs"),
            "a bad db_url must degrade to empty-then-parse, still serving the on-disk file: {roots:?}"
        );
        handle.shutdown().await;
    }

    /// AC#4 sibling (schema fail-soft). A valid `sqlite:` DB whose `nodes` is a **VIEW**
    /// opens cleanly (WAL pragma succeeds) but fails
    /// [`Storage::ensure_schema`](crate::storage::Storage::ensure_schema) — its
    /// `CREATE INDEX … ON nodes(…)` cannot index a view. The run must degrade to
    /// empty-then-parse (no persistence task) and still serve the on-disk file, never
    /// panicking — proving the schema-error branch of the rehydrate boot is fail-soft.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_error_degrades_to_empty_then_parse() {
        let db_dir = tempdir().expect("db tempdir");
        let db_path = db_dir.path().join("conflict.db");
        let db_url = format!("sqlite://{}", db_path.display());

        // Pre-create a valid SQLite DB whose `nodes` is a VIEW: connect (real DB) and the
        // WAL pragma succeed, but ensure_schema's index-on-`nodes` cannot index a view, so
        // ensure_schema errors — the connect-ok / schema-fail branch (not an open error).
        let setup = sqlx::SqlitePool::connect(&format!("{db_url}?mode=rwc"))
            .await
            .expect("create conflict db");
        sqlx::query("CREATE VIEW nodes AS SELECT 1 AS x")
            .execute(&setup)
            .await
            .expect("create conflicting view");
        setup.close().await;

        let repo = tempdir().expect("repo tempdir");
        std::fs::write(repo.path().join("survives.rs"), "fn survives() {}").expect("write");

        let handle = run_with_db_url(repo.path().to_path_buf(), local(), Some(db_url))
            .await
            .expect("run must not fail on a schema error");
        assert!(
            handle.store_task.is_none(),
            "a schema error must disable persistence (no store task)"
        );

        let (mut ws, _) = connect_async(format!("ws://{}/", handle.addr))
            .await
            .expect("connect");
        let env = next_envelope(&mut ws).await;
        let roots = snapshot_root_ids(&env);
        assert!(
            roots.iter().any(|id| id == "file:survives.rs"),
            "a schema error must degrade to empty-then-parse, still serving the on-disk file: {roots:?}"
        );
        handle.shutdown().await;
    }
}
