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
//! the pump, and the collector.
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
use crate::watcher::{is_source_file, watch};
use crate::wire::EventEnvelope;
use crate::ws::{serve, BoundServer};

/// Capacity of the broadcast channel fanning patch events out to WS clients.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// A running Lattice backend: the WebSocket server plus the watcher pump.
///
/// Holds the bound server [`addr`](RunHandle::addr) (read it to connect) and the
/// background tasks. [`RunHandle::shutdown`] stops the server and the watcher.
pub struct RunHandle {
    /// The address the WebSocket server is listening on.
    pub addr: SocketAddr,
    server: BoundServer,
    watcher_task: JoinHandle<()>,
    pump_task: JoinHandle<()>,
    collector_task: JoinHandle<()>,
}

impl RunHandle {
    /// Stops the WebSocket server, the watcher pump, and the CLV collector and
    /// waits for teardown.
    pub async fn shutdown(self) {
        self.server.shutdown().await;
        self.watcher_task.abort();
        self.pump_task.abort();
        self.collector_task.abort();
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
/// Canonicalises `root`, does an initial parse of every source file (Rust, Python,
/// or TypeScript) into a fresh [`Graph`], starts the WebSocket [`serve`]r, and
/// spawns a watcher pump that
/// re-parses changed files and broadcasts their patch events. Pass `127.0.0.1:0`
/// for an ephemeral port and read [`RunHandle::addr`] back.
///
/// # Errors
/// Returns an [`std::io::Error`] if `root` cannot be canonicalised or the server
/// cannot bind to `addr`.
pub async fn run(root: PathBuf, addr: SocketAddr) -> std::io::Result<RunHandle> {
    let root = std::fs::canonicalize(&root)?;
    let graph = Arc::new(Mutex::new(Graph::new()));
    let (events_tx, _) = broadcast::channel::<EventEnvelope>(EVENT_CHANNEL_CAPACITY);

    // Initial parse: fill the graph so the first snapshot reflects the repo.
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher::DEBOUNCE;
    use crate::wire::{EventType, Node, NodeStatus, Payload, TestOutcome};
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
}
