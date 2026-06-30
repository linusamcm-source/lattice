//! CLV collector — tails the sink file, correlates events, and recolours nodes.
//!
//! The collector is the live-observability seam of Phase 5 (`BUILD_PLAN.md` Phase 5,
//! `docs/sprints/phase-5-clv-collector.md` §P5-4). [`collect`] is a `tokio` task
//! that **tails** the per-repo sink file `<root>/.lattice/clv.ndjson` — one
//! `#CLV1 {json}` line per event, appended by an external emitter (a Claude Code
//! `PostToolUse` hook or a test reporter). Each newly appended complete line is
//! parsed via [`parse_clv_line`], folded onto the live graph through
//! [`Graph::apply_clv`], and **every** patch envelope it returns is broadcast on
//! `events_tx` so connected clients update — a `test`/`status` recolour, a `hotedge`
//! heat toggle, or (Phase 8) the agent node/edge upserts plus
//! `agent.activity`/`agent.roster` of an `activity` event. Because the collector has
//! no process-exit signal, every tick also runs [`Graph::expire_idle`]: a process
//! quiet for longer than [`ROSTER_IDLE_MS`](crate::graph::ROSTER_IDLE_MS) is timed
//! out to `inactive` and its `agent.roster` re-broadcast — even on an otherwise idle
//! sink — closing the roster lifecycle. It is wired into [`run`](crate::app::run)
//! alongside the watcher pump and torn down by
//! [`RunHandle::shutdown`](crate::app::RunHandle::shutdown).
//!
//! ## Sink-path-from-root contract
//! The sink path is derived solely from the watched repo root:
//! `root.join(`[`SINK_RELATIVE`]`)`. The emitter MUST write node ids relative to
//! that same root (`file:<relpath>`, `fn:<relpath>:<symbol>`) so they equal
//! Lattice's `node_id` for the repo; an id absent from the graph is ignored by
//! [`Graph::apply_clv`] (no colour, no error).
//!
//! ## Follow / poll semantics (deliberate choice)
//! Following is done by **polling the file length** every [`POLL_INTERVAL`]
//! (200 ms, well under the ~1 s reddening budget) rather than with `notify`.
//! Polling is simpler and more robust for an append-only log: there is no
//! callback-thread bridging, no event coalescing to reason about, and a missed
//! filesystem event cannot strand buffered bytes — each poll re-reads from the last
//! byte offset. The collector keeps two pieces of tail state:
//! - a byte `offset` of how much of the file it has already consumed, and
//! - a `buffer` of bytes read past the last newline (a trailing **partial** line),
//!   held until its terminating `\n` arrives so a line split across two writes is
//!   parsed exactly once, only once complete.
//!
//! On each poll it opens the sink (tolerating absence — the file may be created
//! after startup), reads everything from `offset` to end, advances `offset`, and
//! drains every complete (`\n`-terminated) line from the buffer. If the file's
//! length is **less than** `offset` the file was truncated or rotated, so the
//! collector resets `offset` and clears the buffer to re-read from the start.
//!
//! ## Concurrent sessions
//! The collector holds **no** per-session state: it applies each event to the
//! `node` id the line carries, so interleaved sessions writing the same sink never
//! cross-colour. Two different sessions touching the *same* node race to
//! last-write-wins, which is the correct outcome.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::broadcast;
use tokio::sync::Mutex;

use crate::clv::parse_clv_line;
use crate::graph::Graph;
use crate::wire::EventEnvelope;

/// Sink path relative to the watched repo root (`<root>/.lattice/clv.ndjson`).
///
/// The single source of the collector's sink-path-from-root contract: [`collect`]
/// joins this onto the (already canonicalised) root and tails nothing else.
pub const SINK_RELATIVE: &str = ".lattice/clv.ndjson";

/// Interval between sink-growth polls while following the file.
///
/// 200 ms keeps the worst-case detection latency well under the ~1 s budget for a
/// failing test to redden its node, while staying cheap enough to poll an
/// append-only file indefinitely.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Tails `<root>/.lattice/clv.ndjson`, folding each CLV `test`/`status` event into
/// live node colour and broadcasting the resulting patch.
///
/// Runs until the task is aborted (by [`RunHandle::shutdown`](crate::app::RunHandle::shutdown)).
/// Each [`POLL_INTERVAL`] it runs one [`tick`] with a fresh
/// [`monotonic_now_ms`](crate::graph::monotonic_now_ms) reading: [`tick`] reads newly
/// appended complete lines, parses them with [`parse_clv_line`], and for every parsed
/// event locks `graph` and calls [`Graph::apply_clv`] — **every** [`EventEnvelope`] in
/// the returned vector is sent on `events_tx` for the WebSocket layer to fan out — then
/// sweeps idle processes via [`Graph::expire_idle`]. The file may be absent at startup and created later;
/// a truncation/rotation resets the read offset. Malformed or untagged lines parse
/// to [`None`] and are skipped silently — the tail continues. Panic-free: every I/O
/// error simply ends the current poll and the loop retries on the next tick.
pub async fn collect(
    root: PathBuf,
    graph: Arc<Mutex<Graph>>,
    events_tx: broadcast::Sender<EventEnvelope>,
) {
    let sink = root.join(SINK_RELATIVE);
    let mut offset: u64 = 0;
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        tick(
            &sink,
            &mut offset,
            &mut buffer,
            &graph,
            &events_tx,
            crate::graph::monotonic_now_ms(),
        )
        .await;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Runs one collector iteration: drain new sink lines, then expire idle processes.
///
/// The per-tick body of [`collect`]'s loop, split out so the idle-timeout sweep runs
/// on **every** tick regardless of sink growth. First [`poll_once`] drains and folds
/// any newly appended lines; then `graph` is locked and [`Graph::expire_idle`] is
/// called with `now_ms` — flipping any process quiet for longer than
/// [`ROSTER_IDLE_MS`](crate::graph::ROSTER_IDLE_MS) to `inactive` — and every
/// returned `agent.roster` envelope is broadcast on `events_tx`. Crucially this is
/// **not** gated behind [`poll_once`]'s no-growth early return, so a process that
/// goes quiet on an otherwise idle sink is still timed out. [`collect`] passes a real
/// [`monotonic_now_ms`](crate::graph::monotonic_now_ms) reading each iteration; tests
/// inject `now_ms`. Panic-free: a send to a closed channel is ignored.
async fn tick(
    sink: &Path,
    offset: &mut u64,
    buffer: &mut Vec<u8>,
    graph: &Arc<Mutex<Graph>>,
    events_tx: &broadcast::Sender<EventEnvelope>,
    now_ms: u64,
) {
    poll_once(sink, offset, buffer, graph, events_tx).await;
    let expired = graph.lock().await.expire_idle(now_ms);
    for envelope in expired {
        let _ = events_tx.send(envelope);
    }
}

/// Reads newly appended bytes from `sink` and processes every complete line.
///
/// Updates the tail state in place: `offset` advances by the number of bytes read,
/// and `buffer` retains any trailing partial line (bytes past the last `\n`) for the
/// next poll. A sink shorter than `offset` is treated as truncation/rotation —
/// `offset` resets to `0` and `buffer` clears so reading restarts from the file
/// start. A missing or unreadable sink, or any I/O error, returns early leaving the
/// state untouched (the next poll retries); it never panics.
async fn poll_once(
    sink: &Path,
    offset: &mut u64,
    buffer: &mut Vec<u8>,
    graph: &Arc<Mutex<Graph>>,
    events_tx: &broadcast::Sender<EventEnvelope>,
) {
    let mut file = match tokio::fs::File::open(sink).await {
        Ok(file) => file,
        // Absent or unreadable: tolerated — try again on the next poll.
        Err(_) => return,
    };
    let len = match file.metadata().await {
        Ok(meta) => meta.len(),
        Err(_) => return,
    };
    if len < *offset {
        // Truncated or rotated: re-read from the start.
        *offset = 0;
        buffer.clear();
    }
    if len == *offset {
        return; // No growth since the last poll.
    }
    if file.seek(SeekFrom::Start(*offset)).await.is_err() {
        return;
    }
    let mut chunk = Vec::new();
    if file.read_to_end(&mut chunk).await.is_err() {
        return;
    }
    *offset += chunk.len() as u64;
    buffer.extend_from_slice(&chunk);

    // Drain every complete (`\n`-terminated) line; keep the trailing partial.
    while let Some(newline) = buffer.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buffer.drain(..=newline).collect();
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim_end_matches(['\n', '\r']);
        if let Some(event) = parse_clv_line(trimmed) {
            // `apply_clv` returns every patch one event produces (a `test`/`status`/
            // `hotedge` event yields at most one; an `activity` event yields the
            // agent node/edge upserts plus `agent.activity`/`agent.roster`). Broadcast
            // them all so connected clients see the full effect.
            let envelopes = graph.lock().await.apply_clv(&event);
            for envelope in envelopes {
                let _ = events_tx.send(envelope);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_rust_source;
    use crate::wire::{EventType, Payload};
    use std::io::Write;
    use tempfile::tempdir;

    /// Builds a graph containing `fn:a.rs:f` (status `Unknown`) for the colour path.
    fn graph_with_function() -> Arc<Mutex<Graph>> {
        let mut graph = Graph::new();
        let _ = graph.apply_parsed(parse_rust_source("a.rs", "fn f() {}"));
        Arc::new(Mutex::new(graph))
    }

    /// Appends raw bytes (no added newline) to `path`, creating it if needed.
    fn append_raw(path: &Path, bytes: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open for append");
        file.write_all(bytes.as_bytes()).expect("write bytes");
    }

    #[tokio::test]
    async fn poll_once_on_absent_file_is_a_noop() {
        let dir = tempdir().expect("tempdir");
        let sink = dir.path().join(".lattice/clv.ndjson");
        let graph = graph_with_function();
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);

        let mut offset = 0;
        let mut buffer = Vec::new();
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;

        assert_eq!(offset, 0, "absent file leaves offset untouched");
        assert!(buffer.is_empty());
        assert!(rx.try_recv().is_err(), "absent file emits nothing");
    }

    #[tokio::test]
    async fn poll_once_emits_for_a_complete_line() {
        let dir = tempdir().expect("tempdir");
        let sink = dir.path().join(".lattice/clv.ndjson");
        let graph = graph_with_function();
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);

        append_raw(
            &sink,
            "#CLV1 {\"event\":\"test\",\"session\":\"s1\",\"node\":\"fn:a.rs:f\",\"outcome\":\"fail\"}\n",
        );

        let mut offset = 0;
        let mut buffer = Vec::new();
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;

        let env = rx.try_recv().expect("a test.result envelope");
        assert_eq!(env.event_type, EventType::TestResult);
        match env.payload {
            Payload::TestResult { node_id, .. } => assert_eq!(node_id, "fn:a.rs:f"),
            other => panic!("expected TestResult, got {other:?}"),
        }
        assert!(buffer.is_empty(), "no partial remains after a full line");
    }

    #[tokio::test]
    async fn poll_once_buffers_a_partial_line_until_its_newline() {
        let dir = tempdir().expect("tempdir");
        let sink = dir.path().join(".lattice/clv.ndjson");
        let graph = graph_with_function();
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);
        let mut offset = 0;
        let mut buffer = Vec::new();

        // First write: a partial line (no newline) — nothing parses yet.
        append_raw(
            &sink,
            "#CLV1 {\"event\":\"test\",\"session\":\"s1\",\"node\":\"fn:a.rs:f\",\"outcome\":\"fail",
        );
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;
        assert!(rx.try_recv().is_err(), "partial line must not emit");
        assert!(!buffer.is_empty(), "partial line is buffered");

        // Second write completes the line — now it parses exactly once.
        append_raw(&sink, "\"}\n");
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;
        let env = rx.try_recv().expect("the completed line emits once");
        assert_eq!(env.event_type, EventType::TestResult);
        assert!(rx.try_recv().is_err(), "exactly one emission");
    }

    #[tokio::test]
    async fn poll_once_skips_malformed_and_continues() {
        let dir = tempdir().expect("tempdir");
        let sink = dir.path().join(".lattice/clv.ndjson");
        let graph = graph_with_function();
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);
        let mut offset = 0;
        let mut buffer = Vec::new();

        append_raw(&sink, "PASS app/foo.test.ts\n#CLV1 {\n");
        append_raw(
            &sink,
            "#CLV1 {\"event\":\"test\",\"session\":\"s1\",\"node\":\"fn:a.rs:f\",\"outcome\":\"fail\"}\n",
        );
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;

        let env = rx.try_recv().expect("the valid line after malformed ones");
        assert_eq!(env.event_type, EventType::TestResult);
        assert!(rx.try_recv().is_err(), "malformed lines emit nothing");
    }

    #[tokio::test]
    async fn poll_once_resets_on_truncation() {
        let dir = tempdir().expect("tempdir");
        let sink = dir.path().join(".lattice/clv.ndjson");
        let graph = graph_with_function();
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);
        let mut offset = 0;
        let mut buffer = Vec::new();

        // Consume two full lines, advancing the offset well past zero.
        let pass_line =
            "#CLV1 {\"event\":\"test\",\"session\":\"s1\",\"node\":\"fn:a.rs:f\",\"outcome\":\"pass\"}\n";
        append_raw(&sink, pass_line);
        append_raw(&sink, pass_line);
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;
        let _ = rx.try_recv().expect("first line emits");
        let _ = rx.try_recv().expect("second line emits");
        let consumed = offset;
        assert!(consumed > 0);

        // Truncate the file to a single shorter line (len < offset): the offset must
        // reset and the new line be read from the start.
        std::fs::write(
            &sink,
            "#CLV1 {\"event\":\"test\",\"session\":\"s1\",\"node\":\"fn:a.rs:f\",\"outcome\":\"fail\"}\n",
        )
        .expect("truncate-rewrite");
        assert!(
            (pass_line.len() as u64) < consumed,
            "rewrite must be shorter than the prior offset to model truncation"
        );
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;
        let env = rx.try_recv().expect("post-truncation line emits");
        match env.payload {
            Payload::TestResult {
                node_id, outcome, ..
            } => {
                assert_eq!(node_id, "fn:a.rs:f");
                assert_eq!(outcome, crate::wire::TestOutcome::Fail);
            }
            other => panic!("expected TestResult, got {other:?}"),
        }
    }

    /// P8-2: a single `activity` line must broadcast **every** envelope
    /// `apply_clv` returns — the agent `node.upsert`, the `authored_by`
    /// `edge.upsert`, the `agent.activity`, and the `agent.roster`. RED until
    /// `poll_once` iterates the widened `Vec<EventEnvelope>` and the agent-layer
    /// side effects exist; today the activity arm is a no-op so nothing is sent.
    #[tokio::test]
    async fn poll_once_broadcasts_every_envelope_from_an_activity() {
        let dir = tempdir().expect("tempdir");
        let sink = dir.path().join(".lattice/clv.ndjson");
        let graph = graph_with_function();
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);

        append_raw(
            &sink,
            "#CLV1 {\"event\":\"activity\",\"agent\":\"tdd-green\",\"session\":\"s1\",\"pid\":48213,\"node\":\"fn:a.rs:f\",\"action\":\"modified\"}\n",
        );

        let mut offset = 0;
        let mut buffer = Vec::new();
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;

        // Drain every broadcast envelope and collect the event types observed.
        let mut types = Vec::new();
        while let Ok(env) = rx.try_recv() {
            types.push(env.event_type);
        }
        assert!(
            types.contains(&EventType::NodeUpsert),
            "agent node.upsert must be broadcast, got {types:?}"
        );
        assert!(
            types.contains(&EventType::EdgeUpsert),
            "authored_by edge.upsert must be broadcast, got {types:?}"
        );
        assert!(
            types.contains(&EventType::AgentActivity),
            "agent.activity must be broadcast, got {types:?}"
        );
        assert!(
            types.contains(&EventType::AgentRoster),
            "agent.roster must be broadcast, got {types:?}"
        );
        assert_eq!(
            types.len(),
            4,
            "exactly the four activity envelopes must be broadcast, got {types:?}"
        );
    }

    /// P8-4: the per-tick collector body must drive `Graph::expire_idle` on
    /// **every** tick, independently of sink growth — so a process that has gone
    /// idle is flipped to `inactive` and its `agent.roster` broadcast even when NO
    /// new lines arrived. This is the seam that proves expiry is driven from the
    /// `collect` loop and is NOT gated behind `poll_once`'s no-growth early return.
    ///
    /// Pinned contract for the GREEN engineer — the loop body is extracted into:
    /// ```ignore
    /// async fn tick(
    ///     sink: &Path,
    ///     offset: &mut u64,
    ///     buffer: &mut Vec<u8>,
    ///     graph: &Arc<Mutex<Graph>>,
    ///     events_tx: &broadcast::Sender<EventEnvelope>,
    ///     now_ms: u64,
    /// )
    /// ```
    /// which runs `poll_once(..)` then locks `graph`, calls `expire_idle(now_ms)`,
    /// and broadcasts every returned envelope. `collect`'s loop calls `tick(..)`
    /// each iteration with a real monotonic-millisecond now. RED until `tick`,
    /// `Graph::expire_idle`, and `Graph::apply_clv_at` exist.
    #[tokio::test]
    async fn tick_expires_idle_process_on_a_quiet_sink() {
        let dir = tempdir().expect("tempdir");
        // Never created → a genuinely quiet sink: poll_once finds no growth.
        let sink = dir.path().join(".lattice/clv.ndjson");

        // Seed a graph with one active process whose last_seen is t0.
        let t0: u64 = 1_000;
        let mut g = Graph::new();
        let _ = g.apply_parsed(parse_rust_source("a.rs", "fn f() {}"));
        let _ = g.apply_clv_at(
            &crate::clv::ClvEvent::Activity {
                session: "s1".to_string(),
                pid: Some(48213),
                agent: Some("tdd-green".to_string()),
                msg: None,
                node: "fn:a.rs:f".to_string(),
                action: "modified".to_string(),
            },
            t0,
        );
        let graph = Arc::new(Mutex::new(g));
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);

        // One tick at a time past the idle window, with a quiet sink: poll_once
        // returns early (no growth), but expire_idle still runs and broadcasts the
        // roster marking the idle process inactive.
        let now = t0 + crate::graph::ROSTER_IDLE_MS + 1;
        let mut offset = 0;
        let mut buffer = Vec::new();
        tick(&sink, &mut offset, &mut buffer, &graph, &tx, now).await;

        let env = rx
            .try_recv()
            .expect("the expiry roster must be broadcast on a quiet sink");
        assert_eq!(
            env.event_type,
            EventType::AgentRoster,
            "the broadcast envelope is an agent.roster, got {env:?}"
        );
        match env.payload {
            Payload::AgentRoster { agents, .. } => {
                let row = agents
                    .iter()
                    .find(|a| a.process_id == 48213)
                    .unwrap_or_else(|| panic!("expected a roster row for 48213, got {agents:?}"));
                assert_eq!(
                    row.status, "inactive",
                    "the idle process is flipped to inactive on the tick"
                );
            }
            other => panic!("expected AgentRoster, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn aborting_collect_cancels_the_task() {
        let dir = tempdir().expect("tempdir");
        let graph = graph_with_function();
        let (tx, _rx) = broadcast::channel::<EventEnvelope>(16);
        let task = tokio::spawn(collect(dir.path().to_path_buf(), graph, tx));

        // The task is a long-running poll loop; aborting it cancels it cleanly so
        // no collector task is leaked (the guarantee RunHandle::shutdown relies on).
        task.abort();
        let joined = task.await;
        assert!(joined.is_err(), "abort yields a JoinError");
        assert!(
            joined.unwrap_err().is_cancelled(),
            "the collector task was cancelled by abort"
        );
    }
}
