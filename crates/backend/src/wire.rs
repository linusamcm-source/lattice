//! CLV wire contract — the JSON-over-WebSocket types Lattice streams to clients.
//!
//! This module is the single interop source of truth for the Phase-0 protocol. It
//! mirrors `docs/orignal_specs/DATA_MODEL.md`:
//! - §A.1 — deterministic id convention, via [`node_id`], [`edge_id`], and the
//!   kind-qualified [`typed_edge_id`] (a deliberate §A.1 extension for edges that
//!   share an ordered pair but differ in `kind`).
//! - §A.2 — [`Node`] (structural graph vertex).
//! - §A.3 — [`Edge`] (typed relation between nodes).
//! - §A.4 — [`EventEnvelope`] (`{ v, ts, sessionId, type, payload }`), tagged by
//!   [`EventType`].
//!
//! It also pins the wire payload contract as the typed [`Payload`] variants: the
//! Phase-0 `snapshot`/`node.upsert`/`node.remove`/`edge.upsert`/`edge.remove`
//! diff set, plus the Phase-1 lazy-hierarchy `subtree` reply (the direct children
//! of an expanded node — see [`Payload::Subtree`]). Per `AGENT_PROTOCOL.md` §5 the
//! protocol is identified on the CLV stdio channel by the `#CLV1` sentinel (see
//! [`crate::protocol_sentinel`]).
//!
//! All JSON keys are camelCase and every enum serialises to the exact string the
//! spec mandates, so the Rust backend and the TypeScript client agree byte-for-byte.
//! The [`Node`] modelled here carries the Phase-0 subset of §A.2 (structural fields,
//! optional `docs`/`signature`, and a [`Meta`] holding `language`/`filePath`/`range`);
//! the §A.2 `meta.lastTouchedBy`/`meta.git` agent-and-git sub-objects belong to later
//! phases and are intentionally deferred. Unknown incoming fields are ignored, so
//! adding them later is forward-compatible.

use serde::{Deserialize, Serialize};

/// Kind of graph vertex (`DATA_MODEL.md` §A.2 `type`).
///
/// The serde string form is the spec word (`"function"`, `"file"`, …); the id
/// token used by [`node_id`] is the shorter [`NodeType::id_prefix`] (`"fn"`,
/// `"file"`, …) per the §A.1 id convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeType {
    /// A deployable service / top-level system boundary.
    Service,
    /// A source module or namespace.
    Module,
    /// A single source file.
    File,
    /// A function or method.
    Function,
    /// A variable or binding.
    Variable,
    /// A test case.
    Test,
    /// An agent participating in the session (agent layer).
    Agent,
}

impl NodeType {
    /// Returns the short id token this type contributes to a node id.
    ///
    /// This is the leading `type` segment of the §A.1 id form
    /// `type:path:symbol` — e.g. [`NodeType::Function`] yields `"fn"`, so
    /// `node_id(Function, "src/x.rs", "foo")` is `"fn:src/x.rs:foo"`. It is
    /// deliberately distinct from the serde string form (which is the full spec
    /// word) and is stable across runs so ids keep their identity.
    pub fn id_prefix(self) -> &'static str {
        match self {
            NodeType::Service => "svc",
            NodeType::Module => "mod",
            NodeType::File => "file",
            NodeType::Function => "fn",
            NodeType::Variable => "var",
            NodeType::Test => "test",
            NodeType::Agent => "agent",
        }
    }
}

/// Live state of a node (`DATA_MODEL.md` §A.2 `status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    /// Not yet analysed (the Phase-0 default for freshly parsed nodes).
    Unknown,
    /// Covered by tests that are currently passing.
    Passing,
    /// Covered by tests that are currently failing.
    Failing,
    /// Currently executing (runtime tracer).
    Running,
    /// Known but out of date relative to its source.
    Stale,
    /// Could not be analysed (e.g. the source file failed to parse).
    Error,
}

/// Kind of relation an [`Edge`] expresses (`DATA_MODEL.md` §A.3 `kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Control flow: caller to callee.
    Calls,
    /// A module import.
    Imports,
    /// Structural containment (mirrors `parentId`/`childIds`).
    Contains,
    /// A node to the tests that cover it.
    TestedBy,
    /// A code node to the agent that last touched it (agent layer).
    AuthoredBy,
    /// This function's input originates from the target's return value.
    ParamSource,
    /// This function's output flows into the target.
    DataFlowsFrom,
}

/// Discriminator for an [`EventEnvelope`] (`DATA_MODEL.md` §A.4 `type`).
///
/// Phase 0 emits the structural-diff subset (`snapshot`, `node.upsert`,
/// `node.remove`, `edge.upsert`, `edge.remove`); Phase 1 adds `subtree` (the lazy
/// reply to an `expand` request). The remaining variants are part of the full
/// §A.4 vocabulary and exist so the contract is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventType {
    /// Insert-or-update a node (`payload` is [`Payload::NodeUpsert`]).
    #[serde(rename = "node.upsert")]
    NodeUpsert,
    /// Remove a node by id (`payload` is [`Payload::NodeRemove`]).
    #[serde(rename = "node.remove")]
    NodeRemove,
    /// Insert-or-update an edge (`payload` is [`Payload::EdgeUpsert`]).
    #[serde(rename = "edge.upsert")]
    EdgeUpsert,
    /// Remove an edge by id (`payload` is [`Payload::EdgeRemove`]).
    #[serde(rename = "edge.remove")]
    EdgeRemove,
    /// A node status change (later phases).
    #[serde(rename = "status.update")]
    StatusUpdate,
    /// A test outcome (later phases).
    #[serde(rename = "test.result")]
    TestResult,
    /// An agent action (agent layer).
    #[serde(rename = "agent.activity")]
    AgentActivity,
    /// A runtime call edge going hot or cold (runtime tracer).
    #[serde(rename = "hot_edge")]
    HotEdge,
    /// The live agent roster (agent layer).
    #[serde(rename = "agent.roster")]
    AgentRoster,
    /// The top-level (root-only) graph, sent on connect / resync.
    #[serde(rename = "snapshot")]
    Snapshot,
    /// A node's direct children, replying to a client `expand` request (Phase 1).
    #[serde(rename = "subtree")]
    Subtree,
    /// An out-of-band error notification.
    #[serde(rename = "error")]
    Error,
}

/// One typed parameter of a function [`Signature`] (`DATA_MODEL.md` §A.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    /// Parameter name.
    pub name: String,
    /// Declared parameter type, rendered as source text.
    #[serde(rename = "type")]
    pub param_type: String,
}

/// A function's signature (`DATA_MODEL.md` §A.2 `signature`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Ordered parameter list.
    pub params: Vec<Param>,
    /// Return type, rendered as source text.
    pub returns: String,
}

/// A source span (`DATA_MODEL.md` §A.2 `meta.range`), 1-based line, 0-based column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Range {
    /// First line of the span (1-based).
    pub start_line: u32,
    /// Column where the span starts (0-based).
    pub start_col: u32,
    /// Last line of the span (1-based).
    pub end_line: u32,
    /// Column where the span ends (0-based).
    pub end_col: u32,
}

/// Per-node metadata (`DATA_MODEL.md` §A.2 `meta`), Phase-0 subset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    /// Source language (e.g. `"rust"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Repo-relative path of the owning file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Source span of the node, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

/// A graph vertex (`DATA_MODEL.md` §A.2).
///
/// `id` is built from [`node_id`] so the same element keeps its identity across
/// runs. JSON keys are camelCase (`parentId`, `childIds`); [`PartialEq`] makes the
/// type round-trip-testable through `serde_json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    /// Deterministic id (`type:path:symbol`), from [`node_id`].
    pub id: String,
    /// Vertex kind; serialises as the spec word (e.g. `"function"`).
    #[serde(rename = "type")]
    pub node_type: NodeType,
    /// Human-readable display label (typically the symbol name).
    pub label: String,
    /// Id of the containing node, if any (e.g. a function's file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Ids of directly contained children; always present (may be empty).
    #[serde(default)]
    pub child_ids: Vec<String>,
    /// Live state of the node.
    pub status: NodeStatus,
    /// Extracted documentation, when available (later phases).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    /// Function signature, present for `function` nodes (later phases).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
    /// Structural metadata (language, file path, source range).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

/// A typed relation between two nodes (`DATA_MODEL.md` §A.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    /// Deterministic id: [`edge_id`] (`e:source->target`) for `contains` edges,
    /// or the kind-qualified [`typed_edge_id`] (`e:source->target:kind`) for the
    /// `calls` / `param_source` / `data_flows_from` edges that can share a pair.
    pub id: String,
    /// Source node id.
    pub source: String,
    /// Target node id.
    pub target: String,
    /// Relation kind.
    pub kind: EdgeKind,
    /// `true` while the edge is on the executing stack (runtime tracer).
    pub hot: bool,
}

/// Typed `payload` of an [`EventEnvelope`] — the CLV wire payload contract.
///
/// Serialised untagged: each variant becomes exactly its inner object (e.g.
/// `{ "node": … }`), because the owning envelope's [`EventType`] is the
/// authoritative discriminator. **Variant order is load-bearing for decode:**
/// [`Payload::Subtree`] is declared before [`Payload::Snapshot`] so a
/// `{parentId,nodes,edges}` object resolves to `Subtree` (its `parentId` is
/// required) while a snapshot falls through; reversing them would mis-decode a
/// subtree as a snapshot. Likewise `node.remove` and `edge.remove` share the
/// identical `{ "id": … }` shape — on the wire they are told apart solely by the
/// envelope `type`, so an untagged decode of a bare `{ "id": … }` resolves to
/// [`Payload::NodeRemove`]; always read [`EventEnvelope::event_type`] to know which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Payload {
    /// `subtree` — a node's **direct** children (Phase-1 lazy `expand` reply).
    ///
    /// Declared **before** [`Payload::Snapshot`] on purpose: `parentId` is a
    /// required field, so untagged decode resolves a `{parentId,nodes,edges}`
    /// object here, while a genuine snapshot (no `parentId`) fails this variant
    /// and falls through to [`Payload::Snapshot`]. Declared the other way round,
    /// a subtree would silently mis-decode as a snapshot (struct variants ignore
    /// unknown fields). `nodes` are `parentId`'s direct children and `edges` are
    /// the `contains` edges from `parentId` to them.
    Subtree {
        /// Id of the expanded parent node.
        #[serde(rename = "parentId")]
        parent_id: String,
        /// The parent's direct child nodes.
        nodes: Vec<Node>,
        /// The `contains` edges from the parent to each direct child.
        edges: Vec<Edge>,
    },
    /// `snapshot` — the top-level (root-only) graph.
    Snapshot {
        /// Every root node (those with no `parentId`).
        nodes: Vec<Node>,
        /// Every edge among the root nodes.
        edges: Vec<Edge>,
    },
    /// `node.upsert` — insert-or-update a single node.
    NodeUpsert {
        /// The node to insert or update.
        node: Node,
    },
    /// `edge.upsert` — insert-or-update a single edge.
    EdgeUpsert {
        /// The edge to insert or update.
        edge: Edge,
    },
    /// `node.remove` — drop the node with this id.
    NodeRemove {
        /// Id of the node to remove.
        id: String,
    },
    /// `edge.remove` — drop the edge with this id.
    EdgeRemove {
        /// Id of the edge to remove.
        id: String,
    },
}

/// A CLV event envelope (`DATA_MODEL.md` §A.4).
///
/// Wraps a [`Payload`] with protocol version, timestamp, session id, and the
/// [`EventType`] discriminator. Serialises with a camelCase `sessionId` key and a
/// `type` key holding the spec string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    /// Protocol version (currently `1`).
    pub v: u32,
    /// ISO-8601 emit timestamp.
    pub ts: String,
    /// Originating session id.
    pub session_id: String,
    /// Event discriminator; selects how `payload` is interpreted.
    #[serde(rename = "type")]
    pub event_type: EventType,
    /// The typed event payload.
    pub payload: Payload,
}

/// Builds a deterministic node id from its type, path, and symbol.
///
/// Implements the `type:path:symbol` form of `DATA_MODEL.md` §A.1 using
/// [`NodeType::id_prefix`] for the leading token. When `symbol` is empty the
/// trailing segment is omitted, yielding `type:path` (used for `file` nodes).
///
/// ```
/// use lattice_backend::wire::{node_id, NodeType};
/// assert_eq!(
///     node_id(NodeType::Function, "src/auth/login.rs", "authenticate"),
///     "fn:src/auth/login.rs:authenticate"
/// );
/// assert_eq!(node_id(NodeType::File, "src/auth/login.rs", ""), "file:src/auth/login.rs");
/// ```
pub fn node_id(node_type: NodeType, path: &str, symbol: &str) -> String {
    if symbol.is_empty() {
        format!("{}:{}", node_type.id_prefix(), path)
    } else {
        format!("{}:{}:{}", node_type.id_prefix(), path, symbol)
    }
}

/// Builds a deterministic edge id from its source and target symbols.
///
/// Implements the `e:<source>-><target>` form of `DATA_MODEL.md` §A.1.
///
/// ```
/// use lattice_backend::wire::edge_id;
/// assert_eq!(edge_id("authenticate", "verify_token"), "e:authenticate->verify_token");
/// ```
pub fn edge_id(src_symbol: &str, dst_symbol: &str) -> String {
    format!("e:{src_symbol}->{dst_symbol}")
}

/// Builds a deterministic, **kind-qualified** edge id, unique per
/// `(source, target, kind)` triple.
///
/// Where [`edge_id`] keys only on the endpoint pair (`e:<source>-><target>`),
/// this appends the edge's serde `kind` string, yielding
/// `e:<source>-><target>:<kind>` (e.g. `e:fn:x.rs:a->fn:x.rs:b:calls`). It is a
/// **deliberate extension of `DATA_MODEL.md` §A.1**, whose `e:<source>-><target>`
/// form contemplates only one edge per ordered pair. The extension exists because
/// a `calls` edge and a `data_flows_from` edge can legitimately share the same
/// ordered pair `X→Y`; the graph stores edges in an id-keyed map, so the
/// unqualified form would let one silently overwrite the other. `contains` edges
/// keep the unqualified [`edge_id`] form; the call / data-flow edge kinds use
/// this. The `<kind>` segment is the [`EdgeKind`] serde string (`calls` /
/// `param_source` / `data_flows_from` / …) and is kept in lock-step with the
/// `#[serde(rename_all = "snake_case")]` mapping (the `typed_edge_id` tests pin it
/// against the serde output).
///
/// ```
/// use lattice_backend::wire::{typed_edge_id, EdgeKind};
/// assert_eq!(
///     typed_edge_id("fn:x.rs:a", "fn:x.rs:b", EdgeKind::Calls),
///     "e:fn:x.rs:a->fn:x.rs:b:calls"
/// );
/// ```
pub fn typed_edge_id(src_symbol: &str, dst_symbol: &str, kind: EdgeKind) -> String {
    let kind_str = match kind {
        EdgeKind::Calls => "calls",
        EdgeKind::Imports => "imports",
        EdgeKind::Contains => "contains",
        EdgeKind::TestedBy => "tested_by",
        EdgeKind::AuthoredBy => "authored_by",
        EdgeKind::ParamSource => "param_source",
        EdgeKind::DataFlowsFrom => "data_flows_from",
    };
    format!("e:{src_symbol}->{dst_symbol}:{kind_str}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn sample_node() -> Node {
        Node {
            id: node_id(NodeType::Function, "src/auth/login.rs", "authenticate"),
            node_type: NodeType::Function,
            label: "authenticate".to_string(),
            parent_id: Some(node_id(NodeType::File, "src/auth/login.rs", "")),
            child_ids: vec![node_id(
                NodeType::Variable,
                "src/auth/login.rs",
                "authenticate:user",
            )],
            status: NodeStatus::Passing,
            docs: Some("Authenticates a user against the stored hash.".to_string()),
            signature: Some(Signature {
                params: vec![Param {
                    name: "creds".to_string(),
                    param_type: "Credentials".to_string(),
                }],
                returns: "Result<Token, AuthError>".to_string(),
            }),
            meta: Some(Meta {
                language: Some("rust".to_string()),
                file_path: Some("src/auth/login.rs".to_string()),
                range: Some(Range {
                    start_line: 12,
                    start_col: 0,
                    end_line: 28,
                    end_col: 1,
                }),
            }),
        }
    }

    fn sample_file_node() -> Node {
        Node {
            id: node_id(NodeType::File, "src/auth/login.rs", ""),
            node_type: NodeType::File,
            label: "login.rs".to_string(),
            parent_id: None,
            child_ids: Vec::new(),
            status: NodeStatus::Unknown,
            docs: None,
            signature: None,
            meta: None,
        }
    }

    fn sample_edge() -> Edge {
        Edge {
            id: edge_id("authenticate", "verify_token"),
            source: "fn:src/auth/login.rs:authenticate".to_string(),
            target: "fn:src/auth/token.rs:verify_token".to_string(),
            kind: EdgeKind::Calls,
            hot: false,
        }
    }

    fn node_upsert_envelope() -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-27T10:32:01.123Z".to_string(),
            session_id: "sess-abc123".to_string(),
            event_type: EventType::NodeUpsert,
            payload: Payload::NodeUpsert {
                node: sample_file_node(),
            },
        }
    }

    fn snapshot_envelope() -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-27T10:32:01.123Z".to_string(),
            session_id: "sess-abc123".to_string(),
            event_type: EventType::Snapshot,
            payload: Payload::Snapshot {
                nodes: vec![sample_file_node()],
                edges: vec![sample_edge()],
            },
        }
    }

    fn subtree_envelope() -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-27T10:32:01.123Z".to_string(),
            session_id: "sess-abc123".to_string(),
            event_type: EventType::Subtree,
            payload: Payload::Subtree {
                parent_id: node_id(NodeType::File, "src/auth/login.rs", ""),
                nodes: vec![sample_node()],
                edges: vec![sample_edge()],
            },
        }
    }

    #[test]
    fn node_id_for_function_uses_fn_prefix() {
        assert_eq!(
            node_id(NodeType::Function, "src/auth/login.rs", "authenticate"),
            "fn:src/auth/login.rs:authenticate"
        );
    }

    #[test]
    fn node_id_for_file_omits_empty_symbol() {
        assert_eq!(
            node_id(NodeType::File, "src/auth/login.rs", ""),
            "file:src/auth/login.rs"
        );
    }

    #[test]
    fn edge_id_joins_symbols_with_arrow() {
        assert_eq!(
            edge_id("authenticate", "verify_token"),
            "e:authenticate->verify_token"
        );
    }

    #[test]
    fn typed_edge_id_appends_kind_segment() {
        assert_eq!(
            typed_edge_id("fn:x.rs:a", "fn:x.rs:b", EdgeKind::Calls),
            "e:fn:x.rs:a->fn:x.rs:b:calls"
        );
    }

    #[test]
    fn typed_edge_id_distinguishes_kinds_on_the_same_pair() {
        let calls = typed_edge_id("fn:x.rs:a", "fn:x.rs:b", EdgeKind::Calls);
        let param = typed_edge_id("fn:x.rs:a", "fn:x.rs:b", EdgeKind::ParamSource);
        let flow = typed_edge_id("fn:x.rs:a", "fn:x.rs:b", EdgeKind::DataFlowsFrom);
        assert_ne!(calls, param);
        assert_ne!(calls, flow);
        assert_ne!(param, flow);
    }

    #[test]
    fn typed_edge_id_kind_segment_matches_serde_string() {
        let kinds = [
            EdgeKind::Calls,
            EdgeKind::Imports,
            EdgeKind::Contains,
            EdgeKind::TestedBy,
            EdgeKind::AuthoredBy,
            EdgeKind::ParamSource,
            EdgeKind::DataFlowsFrom,
        ];
        for kind in kinds {
            let serde_str = serde_json::to_value(kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .expect("EdgeKind serialises to a JSON string");
            assert_eq!(typed_edge_id("a", "b", kind), format!("e:a->b:{serde_str}"));
        }
    }

    #[test]
    fn node_serializes_with_camel_case_keys() {
        let value = serde_json::to_value(sample_node()).expect("serialize node");
        let obj = value.as_object().expect("node is a JSON object");
        assert!(
            obj.contains_key("parentId"),
            "expected parentId key: {value}"
        );
        assert!(
            obj.contains_key("childIds"),
            "expected childIds key: {value}"
        );
        assert!(
            !obj.contains_key("parent_id"),
            "snake_case parent_id leaked"
        );
        assert!(
            !obj.contains_key("child_ids"),
            "snake_case child_ids leaked"
        );
    }

    #[test]
    fn node_type_serializes_to_spec_strings() {
        let cases = [
            (NodeType::Service, "service"),
            (NodeType::Module, "module"),
            (NodeType::File, "file"),
            (NodeType::Function, "function"),
            (NodeType::Variable, "variable"),
            (NodeType::Test, "test"),
            (NodeType::Agent, "agent"),
        ];
        for (variant, want) in cases {
            assert_eq!(
                serde_json::to_value(variant).expect("serialize"),
                Value::from(want)
            );
        }
    }

    #[test]
    fn node_type_id_prefixes_are_deterministic() {
        let cases = [
            (NodeType::Service, "svc"),
            (NodeType::Module, "mod"),
            (NodeType::File, "file"),
            (NodeType::Function, "fn"),
            (NodeType::Variable, "var"),
            (NodeType::Test, "test"),
            (NodeType::Agent, "agent"),
        ];
        for (variant, want) in cases {
            assert_eq!(variant.id_prefix(), want);
        }
    }

    #[test]
    fn node_status_serializes_lowercase() {
        let cases = [
            (NodeStatus::Unknown, "unknown"),
            (NodeStatus::Passing, "passing"),
            (NodeStatus::Failing, "failing"),
            (NodeStatus::Running, "running"),
            (NodeStatus::Stale, "stale"),
            (NodeStatus::Error, "error"),
        ];
        for (variant, want) in cases {
            assert_eq!(
                serde_json::to_value(variant).expect("serialize"),
                Value::from(want)
            );
        }
    }

    #[test]
    fn edge_kind_serializes_snake_case() {
        let cases = [
            (EdgeKind::Calls, "calls"),
            (EdgeKind::Imports, "imports"),
            (EdgeKind::Contains, "contains"),
            (EdgeKind::TestedBy, "tested_by"),
            (EdgeKind::AuthoredBy, "authored_by"),
            (EdgeKind::ParamSource, "param_source"),
            (EdgeKind::DataFlowsFrom, "data_flows_from"),
        ];
        for (variant, want) in cases {
            assert_eq!(
                serde_json::to_value(variant).expect("serialize"),
                Value::from(want)
            );
        }
    }

    #[test]
    fn event_type_serializes_to_spec_strings() {
        let cases = [
            (EventType::NodeUpsert, "node.upsert"),
            (EventType::NodeRemove, "node.remove"),
            (EventType::EdgeUpsert, "edge.upsert"),
            (EventType::EdgeRemove, "edge.remove"),
            (EventType::StatusUpdate, "status.update"),
            (EventType::TestResult, "test.result"),
            (EventType::AgentActivity, "agent.activity"),
            (EventType::HotEdge, "hot_edge"),
            (EventType::AgentRoster, "agent.roster"),
            (EventType::Snapshot, "snapshot"),
            (EventType::Subtree, "subtree"),
            (EventType::Error, "error"),
        ];
        for (variant, want) in cases {
            assert_eq!(
                serde_json::to_value(variant).expect("serialize"),
                Value::from(want)
            );
        }
    }

    #[test]
    fn envelope_type_and_node_type_fields_serialize_to_spec_strings() {
        let json = serde_json::to_string(&node_upsert_envelope()).expect("serialize envelope");
        assert!(
            json.contains("\"type\":\"node.upsert\""),
            "missing envelope type: {json}"
        );
        assert!(
            json.contains("\"type\":\"file\""),
            "missing NodeType::File rendering: {json}"
        );
    }

    #[test]
    fn node_upsert_payload_is_object_with_single_node_key() {
        let value = serde_json::to_value(node_upsert_envelope()).expect("serialize");
        let payload = value["payload"].as_object().expect("payload object");
        assert_eq!(payload.len(), 1);
        assert!(payload.contains_key("node"));
    }

    #[test]
    fn snapshot_payload_has_nodes_and_edges_arrays() {
        let value = serde_json::to_value(snapshot_envelope()).expect("serialize");
        let payload = &value["payload"];
        assert!(payload["nodes"].is_array(), "nodes not an array: {payload}");
        assert!(payload["edges"].is_array(), "edges not an array: {payload}");
    }

    #[test]
    fn subtree_envelope_round_trips_and_decodes_as_subtree_not_snapshot() {
        let env = subtree_envelope();
        let json = serde_json::to_string(&env).expect("serialize");

        // Wire shape: the envelope type is "subtree" and the payload carries a
        // camelCase "parentId" key (not snake_case).
        assert!(
            json.contains("\"type\":\"subtree\""),
            "missing subtree envelope type: {json}"
        );
        assert!(
            json.contains("\"parentId\":"),
            "missing camelCase parentId in payload: {json}"
        );
        assert!(
            !json.contains("\"parent_id\""),
            "snake_case parent_id leaked: {json}"
        );

        // Full round-trip: decoding must reproduce the original envelope.
        let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(env, back);

        // Decode-order regression guard: a `{parentId,nodes,edges}` object MUST
        // resolve to Payload::Subtree, NOT silently to Payload::Snapshot.
        assert_eq!(back.event_type, EventType::Subtree);
        assert!(
            matches!(back.payload, Payload::Subtree { .. }),
            "subtree mis-decoded as {:?}",
            back.payload
        );
    }

    #[test]
    fn node_remove_payload_is_single_id_string() {
        let env = EventEnvelope {
            v: 1,
            ts: "2026-06-27T10:32:01.123Z".to_string(),
            session_id: "sess-abc123".to_string(),
            event_type: EventType::NodeRemove,
            payload: Payload::NodeRemove {
                id: "fn:src/x.rs:foo".to_string(),
            },
        };
        let value = serde_json::to_value(env).expect("serialize");
        let payload = value["payload"].as_object().expect("payload object");
        assert_eq!(payload.len(), 1);
        assert_eq!(payload["id"], Value::from("fn:src/x.rs:foo"));
    }

    #[test]
    fn node_round_trips_through_json() {
        let node = sample_node();
        let json = serde_json::to_string(&node).expect("serialize");
        let back: Node = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(node, back);
    }

    #[test]
    fn edge_serializes_expected_shape() {
        let value = serde_json::to_value(sample_edge()).expect("serialize");
        assert_eq!(value["id"], Value::from("e:authenticate->verify_token"));
        assert_eq!(value["kind"], Value::from("calls"));
        assert_eq!(value["hot"], Value::from(false));
    }

    #[test]
    fn envelope_round_trips_for_each_phase0_payload() {
        let envelopes = [
            node_upsert_envelope(),
            snapshot_envelope(),
            EventEnvelope {
                v: 1,
                ts: "2026-06-27T10:32:01.123Z".to_string(),
                session_id: "sess-abc123".to_string(),
                event_type: EventType::NodeRemove,
                payload: Payload::NodeRemove {
                    id: "fn:src/x.rs:foo".to_string(),
                },
            },
            EventEnvelope {
                v: 1,
                ts: "2026-06-27T10:32:01.123Z".to_string(),
                session_id: "sess-abc123".to_string(),
                event_type: EventType::EdgeUpsert,
                payload: Payload::EdgeUpsert {
                    edge: sample_edge(),
                },
            },
        ];
        for env in envelopes {
            let json = serde_json::to_string(&env).expect("serialize");
            let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(env, back);
        }
    }
}
