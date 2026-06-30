//! CLV line parser — decodes one `#CLV1`-tagged stdout line into a typed [`ClvEvent`].
//!
//! Implements the read side of the `AGENT_PROTOCOL.md` §2 wire contract: an agent
//! (or test runner, or runtime tracer) prints `#CLV1 {json}` lines and the collector
//! turns each into a [`ClvEvent`]. The four §2.2 event kinds — `activity`, `test`,
//! `status`, `hotedge` — map to the [`ClvEvent`] variants, carrying their §2.3 fields.
//!
//! The contract is **ignore-malformed**: [`parse_clv_line`] is pure and panic-free
//! and returns [`None`] for any line it cannot decode (no `#CLV1 ` prefix, non-JSON
//! body, missing required field, or unknown `event`). A bad line is skipped and the
//! tail continues — it never errors and never panics, matching the Phase-5
//! "untagged/malformed lines are ignored" requirement (`BUILD_PLAN.md` Phase 5).
//!
//! The `outcome` field of both `test` and `status` reuses the wire
//! [`TestOutcome`] vocabulary (`pass` | `fail` | `skip` | `running`), exactly as
//! §2.3 defines it for both events.

use serde::Deserialize;

use crate::wire::TestOutcome;

/// A parsed CLV event — one decoded `#CLV1` line (`AGENT_PROTOCOL.md` §2).
///
/// Internally tagged on the `event` field (§2.3): the JSON `event` word selects the
/// variant (`activity` → [`ClvEvent::Activity`], `test` → [`ClvEvent::Test`],
/// `status` → [`ClvEvent::Status`], `hotedge` → [`ClvEvent::HotEdge`]). Every variant
/// carries the common §2.3 fields (`session`, and the optional `pid`/`agent`/`msg`)
/// plus the fields that kind requires. Constructed only by [`parse_clv_line`], which
/// yields [`None`] rather than an error for any malformed or unknown line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum ClvEvent {
    /// `activity` — "I touched this code" (§2.2). In Phase 5 this is parsed and
    /// correlated but is a no-op for node colour (attribution is the agent layer).
    Activity {
        /// Originating run id, shared by every agent in the run (§2.3 `session`).
        session: String,
        /// OS process id of the emitter, when provided (§2.3 `pid`).
        pid: Option<u32>,
        /// Stable agent identifier, when provided (§2.3 `agent`).
        agent: Option<String>,
        /// Free-text detail shown on hover, when provided (§2.3 `msg`).
        msg: Option<String>,
        /// Target node id (`type:path:symbol[:child]`, §2.3 `node`).
        node: String,
        /// What happened: `created` | `modified` | `deleted` (§2.3 `action`).
        action: String,
    },
    /// `test` — "a test ran against this node" (§2.2).
    Test {
        /// Originating run id (§2.3 `session`).
        session: String,
        /// OS process id of the emitter, when provided (§2.3 `pid`).
        pid: Option<u32>,
        /// Stable agent identifier, when provided (§2.3 `agent`).
        agent: Option<String>,
        /// Free-text detail, when provided (§2.3 `msg`).
        msg: Option<String>,
        /// Target node id (§2.3 `node`).
        node: String,
        /// Outcome `pass` | `fail` | `skip` | `running` (§2.3 `outcome`), reusing the
        /// wire [`TestOutcome`] vocabulary.
        outcome: TestOutcome,
        /// Test duration in milliseconds, when measured (§2.3 `durationMs`).
        #[serde(rename = "durationMs")]
        duration_ms: Option<u64>,
    },
    /// `status` — "set this node's state directly" (§2.2), e.g. `running` before a
    /// long task.
    Status {
        /// Originating run id (§2.3 `session`).
        session: String,
        /// OS process id of the emitter, when provided (§2.3 `pid`).
        pid: Option<u32>,
        /// Stable agent identifier, when provided (§2.3 `agent`).
        agent: Option<String>,
        /// Free-text detail, when provided (§2.3 `msg`).
        msg: Option<String>,
        /// Target node id (§2.3 `node`).
        node: String,
        /// New node state `pass` | `fail` | `skip` | `running` (§2.3 `outcome`),
        /// reusing the wire [`TestOutcome`] vocabulary.
        outcome: TestOutcome,
    },
    /// `hotedge` — call-path enter/exit (§2.2), usually machine-emitted by the
    /// runtime tracer. [`Graph::apply_clv`](crate::graph::Graph::apply_clv) toggles
    /// the target edge's `hot` flag from this event (enter → hot, exit → cold).
    HotEdge {
        /// Originating run id (§2.3 `session`).
        session: String,
        /// OS process id of the emitter, when provided (§2.3 `pid`).
        pid: Option<u32>,
        /// Stable agent identifier, when provided (§2.3 `agent`).
        agent: Option<String>,
        /// Free-text detail, when provided (§2.3 `msg`).
        msg: Option<String>,
        /// Target edge id (§2.3 `edge`).
        edge: String,
        /// Call-path transition `enter` | `exit` (§2.3 `state`).
        state: String,
    },
}

/// Parses one CLV stdout line into a typed [`ClvEvent`], or [`None`] if the line is
/// not a well-formed CLV event.
///
/// Pure and **panic-free** (the ignore-malformed contract): it returns [`None`] —
/// never an error, never a panic — for every rejected line. A line is rejected when
/// it
/// - does not start with the exact `#CLV1 ` prefix (the sentinel plus its trailing
///   space), e.g. ordinary `cargo test` output;
/// - has a non-JSON body after the prefix (`#CLV1 {not json`);
/// - is a JSON object missing a required field (`event` and `session` for every
///   kind; `node` for `activity`/`test`/`status`; `edge` for `hotedge`); or
/// - carries an unknown `event` value.
///
/// The prefix is matched with [`str::strip_prefix`] (no manual indexing), and the
/// remaining body is decoded with `serde_json`; any decode failure collapses to
/// [`None`].
pub fn parse_clv_line(line: &str) -> Option<ClvEvent> {
    let json = line.strip_prefix("#CLV1 ")?;
    serde_json::from_str(json).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_test_event_with_all_fields() {
        let line = r#"#CLV1 {"event":"test","session":"s1","pid":42,"node":"fn:a.rs:f","outcome":"fail","durationMs":14}"#;
        match parse_clv_line(line) {
            Some(ClvEvent::Test {
                node,
                outcome,
                session,
                pid,
                duration_ms,
                ..
            }) => {
                assert_eq!(node, "fn:a.rs:f");
                assert_eq!(outcome, TestOutcome::Fail);
                assert_eq!(session, "s1");
                assert_eq!(pid, Some(42));
                assert_eq!(duration_ms, Some(14));
            }
            other => panic!("expected Test, got {other:?}"),
        }
    }

    #[test]
    fn parses_real_activity_line() {
        // The exact line verified end-to-end against surf-seer's sink this session.
        let line = r#"#CLV1 {"event":"activity","agent":"claude","session":"dfd7ce20-9761-4e2e-aeac-3b8931cf5d30","pid":17969,"node":"file:src/config/index.ts","action":"modified"}"#;
        match parse_clv_line(line) {
            Some(ClvEvent::Activity {
                node,
                action,
                agent,
                session,
                pid,
                ..
            }) => {
                assert_eq!(node, "file:src/config/index.ts");
                assert_eq!(action, "modified");
                assert_eq!(agent.as_deref(), Some("claude"));
                assert_eq!(session, "dfd7ce20-9761-4e2e-aeac-3b8931cf5d30");
                assert_eq!(pid, Some(17969));
            }
            other => panic!("expected Activity, got {other:?}"),
        }
    }

    #[test]
    fn parses_status_running_event() {
        let line =
            r#"#CLV1 {"event":"status","session":"s1","node":"fn:a.rs:f","outcome":"running"}"#;
        match parse_clv_line(line) {
            Some(ClvEvent::Status {
                outcome,
                node,
                session,
                ..
            }) => {
                assert_eq!(outcome, TestOutcome::Running);
                assert_eq!(node, "fn:a.rs:f");
                assert_eq!(session, "s1");
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn parses_hotedge_event() {
        let line =
            r#"#CLV1 {"event":"hotedge","session":"s1","pid":1,"edge":"e:a->b","state":"enter"}"#;
        match parse_clv_line(line) {
            Some(ClvEvent::HotEdge {
                edge,
                state,
                session,
                ..
            }) => {
                assert_eq!(edge, "e:a->b");
                assert_eq!(state, "enter");
                assert_eq!(session, "s1");
            }
            other => panic!("expected HotEdge, got {other:?}"),
        }
    }

    #[test]
    fn rejects_every_malformed_line() {
        let cases: Vec<&str> = vec![
            // No `#CLV1 ` prefix — ordinary test-runner output.
            "test result: ok. 5 passed",
            "PASS app/foo.test.ts",
            // Sentinel present but the required trailing space is missing.
            "#CLV1{\"event\":\"activity\",\"session\":\"s1\",\"node\":\"file:a.rs\",\"action\":\"modified\"}",
            // Prefix present, body is not JSON.
            "#CLV1 {not json",
            // Prefix present, empty body.
            "#CLV1 ",
            // Valid JSON object, unknown `event`.
            r#"#CLV1 {"event":"bogus","session":"s1","node":"fn:a.rs:f"}"#,
            // Missing `event`.
            r#"#CLV1 {"session":"s1","node":"fn:a.rs:f"}"#,
            // Missing `session` (required for every kind).
            r#"#CLV1 {"event":"test","node":"fn:a.rs:f","outcome":"fail"}"#,
            // Missing `node` for activity/test/status.
            r#"#CLV1 {"event":"activity","session":"s1","action":"modified"}"#,
            r#"#CLV1 {"event":"test","session":"s1","outcome":"fail"}"#,
            r#"#CLV1 {"event":"status","session":"s1","outcome":"running"}"#,
            // Missing `edge` for hotedge.
            r#"#CLV1 {"event":"hotedge","session":"s1","state":"enter"}"#,
            // Valid JSON but not an object (cannot carry an `event` tag).
            r#"#CLV1 ["event","test"]"#,
            r#"#CLV1 42"#,
        ];
        for line in cases {
            assert!(
                parse_clv_line(line).is_none(),
                "expected None for malformed line: {line}"
            );
        }
    }
}
