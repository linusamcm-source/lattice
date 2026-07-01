//! Lattice backend — live code & agent visualiser.
//!
//! Single-binary Rust backend described in `docs/orignal_specs/SPEC.md`. Real
//! functionality is added phase-by-phase per `docs/orignal_specs/BUILD_PLAN.md`.
//!
//! ## Modules
//! - [`wire`] — the CLV JSON-over-WebSocket contract: serde [`wire::Node`],
//!   [`wire::Edge`], and [`wire::EventEnvelope`] types, the payload variants
//!   (Phase-0 diff set, the Phase-1 `subtree` lazy-expand reply, and the Phase-8
//!   `agent.activity` / `agent.roster` agent-layer payloads), and the
//!   deterministic id helpers ([`wire::node_id`] / [`wire::edge_id`] / the
//!   kind-qualified [`wire::typed_edge_id`] / the agent [`wire::agent_node_id`])
//!   that mirror `docs/orignal_specs/DATA_MODEL.md` §A.1–A.5.
//! - [`clv`] — the read side of the `AGENT_PROTOCOL.md` §2 CLV line protocol:
//!   [`clv::parse_clv_line`] decodes one `#CLV1`-tagged stdout line into a typed
//!   [`clv::ClvEvent`] (`activity`/`test`/`status`/`hotedge`), returning [`None`]
//!   panic-free for any untagged, non-JSON, or malformed line (the
//!   ignore-malformed contract).
//! - [`parser`] — source parsers that lower a single file to the structural
//!   [`wire::Node`]/[`wire::Edge`] graph contribution. [`parser::parse_source`] is
//!   the entry point, dispatching on file extension: `syn` for Rust
//!   ([`parser::parse_rust_source`]) and `tree-sitter` for Python and TypeScript;
//!   any other extension yields a bare `file` node. Every parser populates each
//!   node's `docs` from its doc comments (the Rust file node from the module-level
//!   `//!`, each function from its `///`, Python docstrings, TypeScript JSDoc) and
//!   each `function` node's `signature` ([`wire::Signature`]) with its typed
//!   parameters and return type (Rust via `syn`, Python and TypeScript via
//!   `tree-sitter`). The Rust path additionally derives intra-file control-flow
//!   `calls` and data-flow `param_source` / `data_flows_from` edges from function
//!   bodies. All paths recover panic-free from syntax errors.
//! - [`graph`] — the in-memory [`graph::Graph`] holding the current nodes/edges,
//!   rendering a lazy root-only `snapshot`, serving direct children on `expand`
//!   ([`graph::Graph::subtree`]), and diffing a re-parsed file into
//!   `node.*`/`edge.*` patch [`wire::EventEnvelope`]s ([`graph::Graph::apply_parsed`]).
//!   [`graph::Graph::apply_clv`] folds a correlated [`clv::ClvEvent`] onto the live
//!   graph, returning the `Vec` of patch envelopes it produces: a `test`/`status`
//!   event recolours the target node's [`wire::NodeStatus`] (Phase 5) and a `hotedge`
//!   `enter`/`exit` event toggles the target [`wire::Edge`] `hot` flag (Phase 6),
//!   emitting the matching `test.result`/`status.update`/`hot_edge` envelope, while an
//!   attributed `activity` event (Phase 8 agent layer) upserts a root `agent` vertex
//!   and an `authored_by` edge from the touched node, updates the per-process
//!   [`wire::AgentInfo`] roster, and emits `node.upsert`/`edge.upsert`/
//!   `agent.activity`/`agent.roster`. A quiet process is later timed out to
//!   `inactive` by [`graph::Graph::expire_idle`] once it has been silent for the
//!   `ROSTER_IDLE_MS` window, re-emitting the roster only when a status actually
//!   changes (Phase 8). An unknown node/edge id, an unparsable hot-edge state, a
//!   no-change heat transition, or an `activity` event missing its `agent`/`pid` is
//!   a no-op (empty `Vec`). For Phase-9 self-observability it also exposes a metrics
//!   surface: [`graph::Graph::record_parse_latency`] stamps each file's most-recent parse
//!   `duration_us` into a distinct-file-bounded map, and [`graph::Graph::metrics_payload`]
//!   / [`graph::Graph::metrics_envelope`] render a clock-free, deterministic
//!   `metrics.update` (live node/edge counts, a pure `memoryBytes` estimate, events/sec,
//!   and the sorted per-file latencies) — both built over a lock-light
//!   [`graph::Graph::metrics_snapshot`] so the sort and clock stamp run off the graph
//!   mutex.
//! - [`tracing_layer`] — the Phase-6 runtime tracing emitter (the *write* side of
//!   the hot-edge seam): [`tracing_layer::HotEdgeLayer`] is a `tracing` layer that
//!   records an `edge` field off each span and emits a throttled `#CLV1` `hotedge`
//!   `enter`/`exit` line on span enter/close, round-tripping through
//!   [`clv::parse_clv_line`]. Its pure [`tracing_layer::HotEdgeThrottle`] caps
//!   emissions per edge per fixed time window — with independent `enter`/`exit`
//!   sub-budgets so a terminal exit is never starved — so a hot loop cannot flood
//!   the collector. **Transport decision:** the line-based `#CLV1` stdout/sink
//!   transport is kept — no dedicated binary channel — *because* this time-windowed
//!   throttle bounds the per-edge line rate (`enter_cap + exit_cap` per window); the
//!   evidence is the `throttle_bounds_emissions_per_window` throughput-bound test.
//! - [`collector`] — the Phase-5 CLV collector ([`collector::collect`]): a `tokio`
//!   task that tails `<root>/.lattice/clv.ndjson`, parses each newly appended line
//!   via [`clv::parse_clv_line`], and folds the correlated event through
//!   [`graph::Graph::apply_clv`], broadcasting **every** resulting patch
//!   [`wire::EventEnvelope`] (node colour, hot-edge heat, or the Phase-8 agent
//!   node/edge/roster patches) to connected clients. Each tick also sweeps idle
//!   processes to `inactive` via [`graph::Graph::expire_idle`], independent of sink
//!   growth, so a quiet agent's roster status still closes out.
//! - [`storage`] — the Phase-7 persistence seam (`DATA_MODEL.md` §B): a single
//!   async [`storage::Storage`] trait write-throughs the structured CLV
//!   [`wire::EventEnvelope`] stream to one of two interchangeable `sqlx` backends —
//!   SQLite or Postgres — selected by the `LATTICE_DB_URL` scheme
//!   ([`storage::backend_for_url`] / [`storage::open_store`]). Persistence is
//!   **opt-in** (no `LATTICE_DB_URL` ⇒ no database) and **write-through**: the
//!   in-memory [`graph::Graph`] stays the live source of truth for snapshots and
//!   subtrees; storage only records history, and only structured events (never raw
//!   stdout, §B.5).
//! - [`watcher`] — a debounced `notify` filesystem watcher
//!   ([`watcher::watch`]) that forwards changed source-file paths (Rust, Python,
//!   or TypeScript, via [`watcher::is_source_file`]), coalescing rapid bursts
//!   within [`watcher::DEBOUNCE`] and bounding sustained churn via a
//!   [`watcher::MAX_DEBOUNCE`] cap.
//! - [`ws`] — a `tokio-tungstenite` WebSocket server ([`ws::serve`]) that sends
//!   each connecting client the current [`graph::Graph`] root-only `snapshot` and
//!   then streams broadcast [`wire::EventEnvelope`]s, replying to a client
//!   snapshot request with a fresh snapshot and to an `expand` request with the
//!   node's `subtree`.
//! - [`app`] — the wiring entry point [`run`] that joins watcher → parser →
//!   graph → WebSocket so editing a source file (Rust, Python, or TypeScript)
//!   updates a connected client's graph live, and spawns the [`collector`] task so
//!   tailed CLV `test`/`status` events recolour nodes on that same live graph. It also
//!   spawns the Phase-9 self-observability metrics emitter: a task ticking on
//!   [`app::RunConfig::metrics_interval`] that counts each window's broadcast throughput
//!   (its own `metrics.update`s excluded) and broadcasts a `metrics.update` — assembled
//!   off-lock from a brief [`graph::Graph::metrics_snapshot`] — with the live counts, a
//!   deterministic memory estimate, events/sec, and per-file parse latencies.

pub mod app;
pub mod clv;
pub mod collector;
pub mod graph;
pub mod parser;
pub mod storage;
pub mod tracing_layer;
pub mod watcher;
pub mod wire;
pub mod ws;

pub use app::{run, RunHandle};

/// Returns the CLV wire-protocol sentinel this build speaks.
///
/// The sentinel encodes the protocol version (see `AGENT_PROTOCOL.md` §5). The
/// scaffold needs one stable, testable seam so the quality gate (`just qg`) has real
/// code to compile and test against before Phase 0 work begins.
pub fn protocol_sentinel() -> &'static str {
    "#CLV1"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_is_clv1() {
        assert_eq!(protocol_sentinel(), "#CLV1");
    }
}
