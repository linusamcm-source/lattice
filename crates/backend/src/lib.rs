//! Lattice backend — live code & agent visualiser.
//!
//! Single-binary Rust backend described in `docs/orignal_specs/SPEC.md`. Real
//! functionality is added phase-by-phase per `docs/orignal_specs/BUILD_PLAN.md`.
//!
//! ## Modules
//! - [`wire`] — the CLV JSON-over-WebSocket contract: serde [`wire::Node`],
//!   [`wire::Edge`], and [`wire::EventEnvelope`] types, the payload variants
//!   (Phase-0 diff set plus the Phase-1 `subtree` lazy-expand reply), and the
//!   deterministic id helpers ([`wire::node_id`] / [`wire::edge_id`]) that mirror
//!   `docs/orignal_specs/DATA_MODEL.md` §A.1–A.4.
//! - [`parser`] — source parsers that lower a single file to the structural
//!   [`wire::Node`]/[`wire::Edge`] graph contribution. [`parser::parse_source`] is
//!   the entry point, dispatching on file extension: `syn` for Rust
//!   ([`parser::parse_rust_source`]) and `tree-sitter` for Python and TypeScript;
//!   any other extension yields a bare `file` node. All paths recover panic-free
//!   from syntax errors.
//! - [`graph`] — the in-memory [`graph::Graph`] holding the current nodes/edges,
//!   rendering a lazy root-only `snapshot`, serving direct children on `expand`
//!   ([`graph::Graph::subtree`]), and diffing a re-parsed file into
//!   `node.*`/`edge.*` patch [`wire::EventEnvelope`]s ([`graph::Graph::apply_parsed`]).
//! - [`watcher`] — a debounced `notify` filesystem watcher
//!   ([`watcher::watch`]) that forwards changed source-file paths (Rust, Python,
//!   or TypeScript, via [`watcher::is_source_file`]), coalescing rapid bursts
//!   within [`watcher::DEBOUNCE`].
//! - [`ws`] — a `tokio-tungstenite` WebSocket server ([`ws::serve`]) that sends
//!   each connecting client the current [`graph::Graph`] root-only `snapshot` and
//!   then streams broadcast [`wire::EventEnvelope`]s, replying to a client
//!   snapshot request with a fresh snapshot and to an `expand` request with the
//!   node's `subtree`.
//! - [`app`] — the wiring entry point [`run`] that joins watcher → parser →
//!   graph → WebSocket so editing a source file (Rust, Python, or TypeScript)
//!   updates a connected client's graph live.

pub mod app;
pub mod graph;
pub mod parser;
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
