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
//! no process-exit signal, every tick also runs two roster sweeps, both independent of
//! sink growth: [`Graph::expire_idle`] flips a process quiet for longer than
//! [`ROSTER_IDLE_MS`](crate::graph::ROSTER_IDLE_MS) to `inactive`, and
//! [`Graph::reclaim_inactive`] then **removes** any process that has stayed `inactive`
//! beyond the longer [`RETENTION_MS`](crate::graph::RETENTION_MS) window — keeping the
//! roster maps bounded on a long run — re-broadcasting `agent.roster` on any change,
//! even on an otherwise idle sink. It is wired into [`run`](crate::app::run) alongside
//! the watcher pump and torn down by
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
//!   parsed exactly once, only once complete. The buffer is **capped** at
//!   [`MAX_LINE_BYTES`]: an un-terminated line that reaches the cap is over-length and
//!   dropped by resyncing to the next `\n`, so a pathological writer cannot grow it
//!   without bound.
//!
//! On each poll it opens the sink (tolerating absence — the file may be created
//! after startup), reads from `offset` to end **in bounded chunks** (never one
//! unbounded `read_to_end`), advances `offset`, and drains every complete
//! (`\n`-terminated) line from the buffer. If the file's length is **less than**
//! `offset` the file was truncated or rotated, so the collector resets `offset` and
//! clears the buffer to re-read from the start.
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
use crate::wire::{EventEnvelope, EventType};

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

/// Maximum bytes buffered for a single un-terminated sink line before it is dropped.
///
/// A `#CLV1` line is small JSON, so any line whose bytes exceed this cap without a
/// terminating `\n` is treated as **over-length** and discarded by resyncing to the
/// next newline; a line of exactly this many content bytes still parses (the cap is the
/// largest accepted line, not the first rejected size). This bounds [`poll_once`]'s
/// partial-line buffer: without it a writer that appends megabytes with no newline (or
/// a corrupt sink) would grow the buffer unbounded. 64 KiB is far above any legitimate
/// CLV line while keeping the read buffer modest. The over-length resync carries across
/// polls when the offending line spans reads (see [`poll_once`]).
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Tails `<root>/.lattice/clv.ndjson`, folding each CLV `test`/`status` event into
/// live node colour and broadcasting the resulting patch.
///
/// Runs until the task is aborted (by [`RunHandle::shutdown`](crate::app::RunHandle::shutdown)).
/// Each [`POLL_INTERVAL`] it runs one [`tick`] with a fresh
/// [`monotonic_now_ms`](crate::graph::monotonic_now_ms) reading: [`tick`] reads newly
/// appended complete lines, parses them with [`parse_clv_line`], and for every parsed
/// event locks `graph` and calls [`Graph::apply_clv`] — **every** [`EventEnvelope`] in
/// the returned vector is sent on `events_tx` for the WebSocket layer to fan out — then
/// sweeps the roster via [`Graph::expire_idle`] (flip to `inactive`) and
/// [`Graph::reclaim_inactive`] (drop long-idle rows). The file may be absent at startup
/// and created later; a truncation/rotation resets the read offset. Malformed,
/// untagged, or over-length lines parse to [`None`]/are dropped and skipped silently —
/// the tail continues. Panic-free: every I/O error simply ends the current poll and the
/// loop retries on the next tick.
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

/// Runs one collector iteration: drain new sink lines, expire then reclaim processes.
///
/// The per-tick body of [`collect`]'s loop, split out so the roster sweeps run on
/// **every** tick regardless of sink growth. First [`poll_once`] drains and folds any
/// newly appended lines; then `graph` is locked and, in order,
/// [`Graph::expire_idle`] flips any process quiet for longer than
/// [`ROSTER_IDLE_MS`](crate::graph::ROSTER_IDLE_MS) to `inactive`, and
/// [`Graph::reclaim_inactive`] garbage-collects any process that has stayed
/// `inactive` beyond [`RETENTION_MS`](crate::graph::RETENTION_MS) — removing it from
/// the roster so the maps stay bounded. Crucially neither sweep is gated behind
/// [`poll_once`]'s no-growth early return, so a process that goes quiet on an
/// otherwise idle sink is still timed out and eventually reclaimed. [`collect`] passes
/// a real [`monotonic_now_ms`](crate::graph::monotonic_now_ms) reading each iteration;
/// tests inject `now_ms`. Panic-free: a send to a closed channel is ignored.
///
/// **Single roster per tick.** Both sweeps emit a *full* `agent.roster` snapshot on
/// change, so a tick where `expire_idle` flips one pid and `reclaim_inactive` removes
/// another would otherwise broadcast two snapshots. Because `reclaim_inactive` runs
/// second on the same lock, its snapshot already reflects the `expire_idle` flips, so
/// the earlier snapshot is redundant: when `reclaim_inactive` returns a roster the
/// `expire_idle` roster envelope is dropped and only the final post-reclaim snapshot is
/// broadcast (any non-roster envelope is still forwarded).
async fn tick(
    sink: &Path,
    offset: &mut u64,
    buffer: &mut Vec<u8>,
    graph: &Arc<Mutex<Graph>>,
    events_tx: &broadcast::Sender<EventEnvelope>,
    now_ms: u64,
) {
    poll_once(sink, offset, buffer, graph, events_tx).await;
    let roster_events = {
        let mut guard = graph.lock().await;
        let expire_events = guard.expire_idle(now_ms);
        let reclaim_events = guard.reclaim_inactive(now_ms);
        if reclaim_events.is_empty() {
            expire_events
        } else {
            // Reclaim produced the tick's final full-roster snapshot; drop the now-stale
            // `expire_idle` roster snapshot to avoid broadcasting the roster twice, while
            // preserving any non-roster envelope it carried.
            let mut kept: Vec<EventEnvelope> = expire_events
                .into_iter()
                .filter(|env| env.event_type != EventType::AgentRoster)
                .collect();
            kept.extend(reclaim_events);
            kept
        }
    };
    for envelope in roster_events {
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
///
/// **Length-bounded read (Phase 9).** The newly appended region is read in **capped
/// chunks** of at most [`MAX_LINE_BYTES`] (sized to the smaller of the backlog and the
/// cap, never `read_to_end`), so a single poll cannot allocate an arbitrarily large
/// buffer. The partial-line `buffer` is likewise capped: a line of up to exactly
/// [`MAX_LINE_BYTES`] content bytes is buffered and parses normally, but the first byte
/// that would push it **strictly past** the cap marks the line **over-length** — the
/// buffer is pinned at `MAX_LINE_BYTES + 1` (one sentinel byte past the cap, never
/// growing further) and the line is dropped by **resyncing to the next `\n`**. That
/// pinned marker carries across polls when the offending line spans reads, so the
/// following valid `#CLV1` line is still parsed intact.
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
    // Read the appended region in bounded chunks rather than one `read_to_end`, so a
    // huge write can never balloon a single allocation. Size the chunk to the *smaller*
    // of the appended region and [`MAX_LINE_BYTES`], so the common case (a few small
    // lines) allocates and zeroes only what it needs instead of a full 64 KiB every
    // poll; a larger backlog is drained by reusing this same buffer across loop turns.
    let appended = len.saturating_sub(*offset) as usize;
    let chunk_size = appended.clamp(1, MAX_LINE_BYTES);
    let mut chunk = vec![0u8; chunk_size];
    loop {
        let read = match file.read(&mut chunk).await {
            Ok(0) => break, // End of the appended region.
            Ok(n) => n,
            // I/O error mid-read: stop; `offset` already reflects consumed bytes and
            // the next poll retries.
            Err(_) => return,
        };
        *offset += read as u64;
        ingest_bytes(&chunk[..read], buffer, graph, events_tx).await;
    }
}

/// Folds newly read `data` into the capped partial-line `buffer`, forwarding lines.
///
/// Splits `data` on `\n` and processes each complete line. When `buffer` is empty the
/// whole line is already contiguous in `data`, so it is parsed **in place** with no
/// copy (the zero-copy fast path); only a trailing partial (bytes past the last `\n`) is
/// copied into `buffer` to await its terminator on a later read. A line carried over
/// from a previous read (`buffer` non-empty) is stitched with its completing bytes and
/// then parsed. Each parsed line goes through [`dispatch_line`].
///
/// **Strict over-length cap.** A line of up to exactly [`MAX_LINE_BYTES`] content bytes
/// is accepted and parsed; a line that would exceed the cap is **over-length** and
/// dropped. An over-length *partial* pins `buffer` at `MAX_LINE_BYTES + 1` — one sentinel
/// byte past the cap, never growing further — as a structural "resync" marker; every
/// subsequent byte up to and including the next `\n` is then discarded and normal
/// buffering resumes. Because the marker lives in `buffer`, the resync carries across
/// poll boundaries when the over-length line spans reads. Panic-free: lossy UTF-8, and a
/// closed channel send is ignored.
async fn ingest_bytes(
    data: &[u8],
    buffer: &mut Vec<u8>,
    graph: &Arc<Mutex<Graph>>,
    events_tx: &broadcast::Sender<EventEnvelope>,
) {
    let mut rest = data;
    while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
        let (head, tail) = rest.split_at(newline + 1);
        let content = &head[..newline]; // line bytes without the `\n`.
        if buffer.is_empty() {
            // Zero-copy fast path: the whole line is contiguous in `data`. A line of up
            // to exactly the cap parses; one strictly longer is over-length and dropped.
            if content.len() <= MAX_LINE_BYTES {
                dispatch_line(content, graph, events_tx).await;
            }
        } else if buffer.len() > MAX_LINE_BYTES {
            // Over-length marker (pinned at cap + 1) set on an earlier read: this `\n`
            // ends the dropped line — reset and resume normal buffering.
            buffer.clear();
        } else if buffer.len() + content.len() <= MAX_LINE_BYTES {
            // A partial carried from an earlier read completes within the cap (exactly
            // the cap included): stitch and parse.
            buffer.extend_from_slice(content);
            dispatch_line(buffer, graph, events_tx).await;
            buffer.clear();
        } else {
            // Completing this line pushes it strictly past the cap: drop, do not parse.
            buffer.clear();
        }
        rest = tail;
    }
    // No newline in the remainder: buffer it. A partial is held up to the cap so an
    // exactly-cap line can still complete; the first byte past the cap pins the buffer at
    // `MAX_LINE_BYTES + 1` as the over-length resync marker for the next read (its content
    // is never parsed — only the next `\n` clears it). At the marker length nothing more
    // is copied, so the buffer never grows past cap + 1.
    if buffer.len() <= MAX_LINE_BYTES {
        let room = (MAX_LINE_BYTES + 1) - buffer.len();
        let take = rest.len().min(room);
        buffer.extend_from_slice(&rest[..take]);
    }
}

/// Parses one complete sink line and broadcasts every envelope it yields.
///
/// Decodes `line` (a single `#CLV1` record without its trailing `\n`, `\r` tolerated)
/// via [`parse_clv_line`]; on a decode it folds the event onto the graph with
/// [`Graph::apply_clv`] and broadcasts **every** returned [`EventEnvelope`] on
/// `events_tx` — a `test`/`status`/`hotedge` event yields at most one, an `activity`
/// event yields the agent node/edge upserts plus `agent.activity`/`agent.roster`.
/// A non-`#CLV1` or malformed line decodes to [`None`] and is skipped. Panic-free: lossy
/// UTF-8, and a send to a closed channel is ignored.
async fn dispatch_line(
    line: &[u8],
    graph: &Arc<Mutex<Graph>>,
    events_tx: &broadcast::Sender<EventEnvelope>,
) {
    let event = {
        let text = String::from_utf8_lossy(line);
        parse_clv_line(text.trim_end_matches(['\n', '\r']))
    };
    if let Some(event) = event {
        let envelopes = graph.lock().await.apply_clv(&event);
        for envelope in envelopes {
            let _ = events_tx.send(envelope);
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

    /// P9-8: the per-tick collector body must drive `Graph::reclaim_inactive` on
    /// **every** tick, right after `expire_idle` and independently of sink growth —
    /// so a process quiet beyond the retention window is *removed* from the roster
    /// even on a genuinely quiet sink (no new lines). This is the seam that proves
    /// reclaim is driven from the `collect` loop and is NOT gated behind
    /// `poll_once`'s no-growth early return. A fresh, recently-active process is
    /// left untouched.
    ///
    /// RED until `crate::graph::RETENTION_MS` and the `tick`-driven
    /// `Graph::reclaim_inactive` exist: today `tick` only calls `expire_idle`, so
    /// the long-idle pid stays in the roster (merely `inactive`) and the retention
    /// constant does not compile (E0425).
    #[tokio::test]
    async fn tick_reclaims_long_idle_process_on_a_quiet_sink() {
        let dir = tempdir().expect("tempdir");
        // Never created → a genuinely quiet sink: poll_once finds no growth.
        let sink = dir.path().join(".lattice/clv.ndjson");

        let t0: u64 = 1_000;
        let mut g = Graph::new();
        let _ = g.apply_parsed(parse_rust_source("a.rs", "fn f() {}"));
        // pid 48213 goes idle at t0 — old enough to be reclaimed at `now`.
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
        let (tx, _rx) = broadcast::channel::<EventEnvelope>(16);

        // `now` is past the retention window relative to t0, so on this tick the
        // idle pid is flipped inactive by expire_idle then reclaimed. A fresh pid
        // touched at `now` must survive untouched.
        let now = t0 + crate::graph::RETENTION_MS + 1;
        graph.lock().await.apply_clv_at(
            &crate::clv::ClvEvent::Activity {
                session: "s1".to_string(),
                pid: Some(99999),
                agent: Some("tdd-fresh".to_string()),
                msg: None,
                node: "fn:a.rs:f".to_string(),
                action: "modified".to_string(),
            },
            now,
        );

        let mut offset = 0;
        let mut buffer = Vec::new();
        tick(&sink, &mut offset, &mut buffer, &graph, &tx, now).await;

        // Inspect the live roster via the public snapshot accessor.
        let agents = match graph.lock().await.roster_snapshot() {
            Some(env) => match env.payload {
                Payload::AgentRoster { agents, .. } => agents,
                other => panic!("expected AgentRoster, got {other:?}"),
            },
            None => Vec::new(),
        };
        assert!(
            !agents.iter().any(|a| a.process_id == 48213),
            "the long-idle pid must be reclaimed from the roster on a quiet sink, got {agents:?}"
        );
        assert!(
            agents.iter().any(|a| a.process_id == 99999),
            "a freshly active pid must be unaffected by reclamation, got {agents:?}"
        );
    }

    /// P9-8: `poll_once` must read in capped chunks and drop an over-length line by
    /// resyncing to the next `\n` — carrying a skip flag across polls when the
    /// over-long line spans two reads — never corrupting the following valid line.
    ///
    /// Feeds an over-long line (longer than `MAX_LINE_BYTES`, no newline) split
    /// across two polls, followed by a valid `#CLV1` line, and asserts only the
    /// valid line is parsed/broadcast and no partial is left carried.
    ///
    /// RED until `super::MAX_LINE_BYTES` and the capped/resyncing read exist: the
    /// constant does not compile today (E0425), and the current `read_to_end` path
    /// would buffer the whole over-long partial unbounded.
    #[tokio::test]
    async fn poll_once_resyncs_over_long_line_split_across_two_polls() {
        let dir = tempdir().expect("tempdir");
        let sink = dir.path().join(".lattice/clv.ndjson");
        let graph = graph_with_function();
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);
        let mut offset = 0;
        let mut buffer = Vec::new();

        // Poll 1: the first slice of the over-long line already exceeds the cap and
        // has no terminating newline. The read must stay bounded (not buffer it all)
        // and set a skip flag for the rest of this line.
        let first = format!("#CLV1 {}", "x".repeat(MAX_LINE_BYTES + 1_000));
        append_raw(&sink, &first);
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;
        assert!(
            rx.try_recv().is_err(),
            "an incomplete over-long line must not emit anything"
        );
        assert!(
            buffer.len() <= MAX_LINE_BYTES + 1,
            "an over-long partial line must be pinned at the cap + 1 sentinel, not buffered \
             unbounded: buffer={} cap={MAX_LINE_BYTES}",
            buffer.len()
        );

        // Poll 2: the tail of the over-long line + its newline (the skip flag must
        // carry across the poll boundary and consume up to this newline), then a
        // valid line that must still parse cleanly.
        append_raw(&sink, &format!("{}\n", "y".repeat(500)));
        append_raw(
            &sink,
            "#CLV1 {\"event\":\"test\",\"session\":\"s1\",\"node\":\"fn:a.rs:f\",\"outcome\":\"fail\"}\n",
        );
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;

        let env = rx
            .try_recv()
            .expect("the valid line following the dropped over-long line must emit");
        assert_eq!(env.event_type, EventType::TestResult);
        match env.payload {
            Payload::TestResult {
                node_id, outcome, ..
            } => {
                assert_eq!(node_id, "fn:a.rs:f");
                assert_eq!(outcome, crate::wire::TestOutcome::Fail);
            }
            other => panic!("expected TestResult, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "the over-long line must produce no envelope of its own"
        );
        assert!(
            buffer.is_empty(),
            "parser not corrupted: no partial is carried after the valid line, got {buffer:?}"
        );
    }

    /// P9-8: a line whose bytes overflow the cap only *once its terminating `\n`
    /// arrives* (a partial buffered below the cap, then a completing write that pushes
    /// it over) is dropped wholesale — the "completed-but-over-cap" path — while the
    /// following valid line still parses. Complements the split-across-polls resync
    /// test by exercising the drop from a below-cap buffer rather than a pinned marker.
    #[tokio::test]
    async fn poll_once_drops_a_completed_line_that_overflows_the_cap() {
        let dir = tempdir().expect("tempdir");
        let sink = dir.path().join(".lattice/clv.ndjson");
        let graph = graph_with_function();
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);
        let mut offset = 0;
        let mut buffer = Vec::new();

        // Poll 1: a partial line just under the cap (no newline) — buffered, not marked.
        append_raw(
            &sink,
            &format!("#CLV1 {}", "x".repeat(MAX_LINE_BYTES - 100)),
        );
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;
        assert!(rx.try_recv().is_err(), "an incomplete line must not emit");
        assert!(
            !buffer.is_empty() && buffer.len() < MAX_LINE_BYTES,
            "the sub-cap partial is buffered below the cap, got {}",
            buffer.len()
        );

        // Poll 2: 200 more bytes + newline push the completed line over the cap (drop),
        // followed by a valid line that must still parse.
        append_raw(&sink, &format!("{}\n", "x".repeat(200)));
        append_raw(
            &sink,
            "#CLV1 {\"event\":\"test\",\"session\":\"s1\",\"node\":\"fn:a.rs:f\",\"outcome\":\"fail\"}\n",
        );
        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;

        let env = rx
            .try_recv()
            .expect("the valid line after the dropped over-cap line must emit");
        assert_eq!(env.event_type, EventType::TestResult);
        assert!(
            rx.try_recv().is_err(),
            "the over-cap line must produce no envelope of its own"
        );
        assert!(buffer.is_empty(), "no partial carried, got {buffer:?}");
    }

    /// Builds an `activity` CLV event attributing `node` `fn:a.rs:f` to `pid`/`agent`.
    fn activity_event(pid: u32, agent: &str) -> crate::clv::ClvEvent {
        crate::clv::ClvEvent::Activity {
            session: "s1".to_string(),
            pid: Some(pid),
            agent: Some(agent.to_string()),
            msg: None,
            node: "fn:a.rs:f".to_string(),
            action: "modified".to_string(),
        }
    }

    /// P9-8 fix: a line of **exactly** [`MAX_LINE_BYTES`] content bytes must still parse
    /// — the cap is the largest accepted line, not the first rejected size. Regression
    /// for the off-by-one where the `>= MAX_LINE_BYTES` marker dropped an exactly-cap
    /// line once it filled the buffer before its `\n` (here the newline lands in a second
    /// read, so the buffer reaches exactly the cap before completing).
    #[tokio::test]
    async fn poll_once_parses_a_line_of_exactly_the_cap() {
        let dir = tempdir().expect("tempdir");
        let sink = dir.path().join(".lattice/clv.ndjson");
        let graph = graph_with_function();
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);
        let mut offset = 0;
        let mut buffer = Vec::new();

        // A valid CLV line padded with trailing spaces (serde_json tolerates them) to
        // exactly MAX_LINE_BYTES content bytes, then its terminating newline.
        let base =
            "#CLV1 {\"event\":\"test\",\"session\":\"s1\",\"node\":\"fn:a.rs:f\",\"outcome\":\"fail\"}";
        assert!(base.len() <= MAX_LINE_BYTES, "base line must fit the cap");
        let mut line = String::with_capacity(MAX_LINE_BYTES + 1);
        line.push_str(base);
        while line.len() < MAX_LINE_BYTES {
            line.push(' ');
        }
        assert_eq!(line.len(), MAX_LINE_BYTES, "content is exactly the cap");
        line.push('\n');
        append_raw(&sink, &line);

        poll_once(&sink, &mut offset, &mut buffer, &graph, &tx).await;

        let env = rx
            .try_recv()
            .expect("a line of exactly the cap must parse and emit");
        assert_eq!(env.event_type, EventType::TestResult);
        match env.payload {
            Payload::TestResult { node_id, .. } => assert_eq!(node_id, "fn:a.rs:f"),
            other => panic!("expected TestResult, got {other:?}"),
        }
        assert!(
            buffer.is_empty(),
            "no partial remains after the exactly-cap line, got {buffer:?}"
        );
    }

    /// P9-8 fix: the zero-copy fast path in [`ingest_bytes`] enforces the strict cap
    /// even for a *contiguous* over-length line (buffer empty) — it is dropped, not
    /// dispatched, and the following valid line still parses. Exercised directly because
    /// [`poll_once`] chunks reads at the cap and never hands `ingest_bytes` a longer
    /// contiguous slice, so this guards the standalone function's own bound.
    #[tokio::test]
    async fn ingest_bytes_drops_a_contiguous_over_length_line_via_fast_path() {
        let graph = graph_with_function();
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);
        let mut buffer = Vec::new();

        // First line's content is `#CLV1 ` + MAX x's = strictly over the cap; the empty
        // buffer routes it through the fast path where the `<= cap` guard drops it.
        let over = format!("#CLV1 {}\n", "x".repeat(MAX_LINE_BYTES));
        let valid =
            "#CLV1 {\"event\":\"test\",\"session\":\"s1\",\"node\":\"fn:a.rs:f\",\"outcome\":\"fail\"}\n";
        let mut data = over.into_bytes();
        data.extend_from_slice(valid.as_bytes());

        ingest_bytes(&data, &mut buffer, &graph, &tx).await;

        let env = rx
            .try_recv()
            .expect("the valid line after the contiguous over-length one must emit");
        assert_eq!(env.event_type, EventType::TestResult);
        assert!(
            rx.try_recv().is_err(),
            "the over-length line must not emit an envelope of its own"
        );
        assert!(buffer.is_empty(), "no partial carried, got {buffer:?}");
    }

    /// P9-8 fix: a tick where `expire_idle` flips one pid AND `reclaim_inactive` removes
    /// a different pid must broadcast a **single** `agent.roster` snapshot — the final
    /// post-reclaim one — not two. Regression against the redundant double-roster
    /// broadcast; correctness-neutral, so the roster still ends with the flipped pid
    /// inactive and the reclaimed pid gone.
    #[tokio::test]
    async fn tick_dedups_double_roster_when_expire_and_reclaim_both_fire() {
        let dir = tempdir().expect("tempdir");
        // Quiet sink (never created) so poll_once contributes nothing this tick.
        let sink = dir.path().join(".lattice/clv.ndjson");

        let now: u64 = 100_000;
        let mut g = Graph::new();
        let _ = g.apply_parsed(parse_rust_source("a.rs", "fn f() {}"));
        // pid A: idle past the idle window but within retention → expire_idle flips it,
        // reclaim leaves it.
        let _ = g.apply_clv_at(
            &activity_event(48213, "tdd-a"),
            now - (crate::graph::ROSTER_IDLE_MS + 1),
        );
        // pid B: idle past the retention window → expire_idle flips it AND reclaim
        // removes it, so both sweeps change the roster on this one tick.
        let _ = g.apply_clv_at(
            &activity_event(99999, "tdd-b"),
            now - (crate::graph::RETENTION_MS + 1),
        );
        let graph = Arc::new(Mutex::new(g));
        let (tx, mut rx) = broadcast::channel::<EventEnvelope>(16);

        let mut offset = 0;
        let mut buffer = Vec::new();
        tick(&sink, &mut offset, &mut buffer, &graph, &tx, now).await;

        // Exactly one agent.roster envelope for the tick — the final post-reclaim one.
        let mut rosters = Vec::new();
        while let Ok(env) = rx.try_recv() {
            if env.event_type == EventType::AgentRoster {
                rosters.push(env);
            }
        }
        assert_eq!(
            rosters.len(),
            1,
            "expire_idle + reclaim in one tick must broadcast a single (final) roster, \
             got {}",
            rosters.len()
        );
        match rosters.into_iter().next().map(|e| e.payload) {
            Some(Payload::AgentRoster { agents, .. }) => {
                assert!(
                    !agents.iter().any(|a| a.process_id == 99999),
                    "the reclaimed pid must be gone from the final roster, got {agents:?}"
                );
                let a = agents
                    .iter()
                    .find(|a| a.process_id == 48213)
                    .unwrap_or_else(|| panic!("expected the retained pid 48213, got {agents:?}"));
                assert_eq!(a.status, "inactive", "the retained pid stays inactive");
            }
            other => panic!("expected AgentRoster payload, got {other:?}"),
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
