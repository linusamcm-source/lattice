//! Lattice backend — live code & agent visualiser.
//!
//! Single-binary Rust backend described in `docs/orignal_specs/SPEC.md`. This crate
//! currently holds only the walking-skeleton scaffold; real functionality is added
//! phase-by-phase per `docs/orignal_specs/BUILD_PLAN.md`.

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
