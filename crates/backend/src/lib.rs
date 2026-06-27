//! Lattice backend — live code & agent visualiser.
//!
//! Single-binary Rust backend described in `docs/orignal_specs/SPEC.md`. Real
//! functionality is added phase-by-phase per `docs/orignal_specs/BUILD_PLAN.md`.
//!
//! ## Modules
//! - [`wire`] — the CLV JSON-over-WebSocket contract: serde [`wire::Node`],
//!   [`wire::Edge`], and [`wire::EventEnvelope`] types, the Phase-0 payload
//!   variants, and the deterministic id helpers ([`wire::node_id`] /
//!   [`wire::edge_id`]) that mirror `docs/orignal_specs/DATA_MODEL.md` §A.1–A.4.
//! - [`parser`] — a `syn`-based Rust source parser that lowers a single file to
//!   the structural [`wire::Node`]/[`wire::Edge`] graph contribution
//!   ([`parser::parse_rust_source`]), recovering panic-free from syntax errors.
//! - [`graph`] — the in-memory [`graph::Graph`] holding the current nodes/edges,
//!   rendering a `snapshot` and diffing a re-parsed file into `node.*`/`edge.*`
//!   patch [`wire::EventEnvelope`]s ([`graph::Graph::apply_parsed`]).
//! - [`watcher`] — a debounced `notify` filesystem watcher
//!   ([`watcher::watch`]) that forwards changed `.rs` file paths, coalescing
//!   rapid bursts within [`watcher::DEBOUNCE`].

pub mod graph;
pub mod parser;
pub mod watcher;
pub mod wire;

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
