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
//! - §A.5 — the test/status CLV event payloads [`Payload::TestResult`] and
//!   [`Payload::StatusUpdate`], the Phase-6 runtime-tracer `hot_edge` payload
//!   [`Payload::HotEdge`] (carrying [`HotEdgeState`]), and the Phase-8 agent-layer
//!   payloads [`Payload::AgentActivity`] and [`Payload::AgentRoster`] (the latter
//!   carrying [`AgentInfo`] rows), whose agent vertices are addressed by
//!   [`agent_node_id`].
//!
//! It also pins the wire payload contract as the typed [`Payload`] variants: the
//! Phase-0 `snapshot`/`node.upsert`/`node.remove`/`edge.upsert`/`edge.remove`
//! diff set, plus the Phase-1 lazy-hierarchy `subtree` reply (the direct children
//! of an expanded node — see [`Payload::Subtree`]), the Phase-5 §A.5
//! `test.result` / `status.update` payloads ([`Payload::TestResult`],
//! [`Payload::StatusUpdate`]), the Phase-6 §A.5 `hot_edge` payload
//! ([`Payload::HotEdge`], toggling an [`Edge::hot`] flag via [`HotEdgeState`]), and
//! the Phase-8 §A.5 agent-layer `agent.activity` / `agent.roster` payloads
//! ([`Payload::AgentActivity`], [`Payload::AgentRoster`]).
//! Per `AGENT_PROTOCOL.md` §5 the
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

/// Outcome of a single test run (`DATA_MODEL.md` §A.5 `test.result.outcome`).
///
/// Serialises snake_case to the exact CLV strings (`"pass"`/`"fail"`/`"skip"`/
/// `"running"`) and is the `outcome` field of [`Payload::TestResult`]. The graph
/// layer maps it onto a [`NodeStatus`] colour (`fail`→`Failing`, `pass`→`Passing`,
/// `skip`→`Stale`, `running`→`Running`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    /// The test passed.
    Pass,
    /// The test failed.
    Fail,
    /// The test was skipped / ignored.
    Skip,
    /// The test is currently executing.
    Running,
}

/// Transition of a runtime call edge between hot and cold (`DATA_MODEL.md` §A.5
/// `hot_edge.state`).
///
/// Serialises lowercase to the exact CLV strings (`"enter"`/`"exit"`) and is the
/// `state` field of [`Payload::HotEdge`]. Per §A.5, `enter` sets the target
/// [`Edge::hot`] to `true` (the edge is on the executing stack); `exit` clears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HotEdgeState {
    /// The call edge went hot — execution entered the callee (`hot:true`).
    Enter,
    /// The call edge went cold — execution left the callee (`hot:false`).
    Exit,
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
/// For the same reason the Phase-5 [`Payload::TestResult`] is declared **before**
/// [`Payload::StatusUpdate`]: a `test.result` object carries every field
/// `StatusUpdate` requires, so the `testId`-bearing variant is tried first or a
/// test result would silently mis-decode as a status update.
///
/// The Phase-8 agent-layer variants [`Payload::AgentActivity`] and
/// [`Payload::AgentRoster`] are declared **last**: each is disambiguated by a
/// required field no earlier variant carries (`action` for `agent.activity`, the
/// `agents` array for `agent.roster`), so they neither mis-decode as an earlier
/// variant nor capture any earlier payload.
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
    /// `test.result` — a test outcome for a node (`DATA_MODEL.md` §A.5).
    ///
    /// Declared **after** the Phase-0/1 variants and **before**
    /// [`Payload::StatusUpdate`], for the same load-bearing reason the
    /// [`Payload::Subtree`]-before-[`Payload::Snapshot`] note above describes: this
    /// enum is `#[serde(untagged)]` and a struct variant ignores unknown fields. A
    /// `test.result` object `{nodeId,testId,outcome,sessionId,…}` carries every
    /// field [`Payload::StatusUpdate`] requires, so were `StatusUpdate` declared
    /// first an untagged decode would resolve a test result as a status update and
    /// silently drop `testId`/`outcome`/`durationMs`. The required `testId` — absent
    /// on `StatusUpdate` — is the disambiguator: a `status.update` object (no
    /// `testId`) fails this variant and falls through to [`Payload::StatusUpdate`].
    TestResult {
        /// Id of the node the test covers.
        #[serde(rename = "nodeId")]
        node_id: String,
        /// Stable test identifier; its presence disambiguates this variant from
        /// [`Payload::StatusUpdate`] under the untagged decode.
        #[serde(rename = "testId")]
        test_id: String,
        /// Pass / fail / skip / running outcome.
        outcome: TestOutcome,
        /// Wall-clock test duration in milliseconds, when measured.
        #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// Originating session id.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Id of the agent that produced the result, when attributed.
        #[serde(rename = "agentId", skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        /// OS process id of the producer, when known.
        #[serde(rename = "processId", skip_serializing_if = "Option::is_none")]
        process_id: Option<u32>,
        /// Optional human-readable failure / skip detail.
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// `status.update` — set a node's live [`NodeStatus`] (`DATA_MODEL.md` §A.5).
    ///
    /// Declared **after** [`Payload::TestResult`]: a `status.update` object carries
    /// no `testId`, so it fails the `testId`-required `TestResult` variant and
    /// resolves here. See the [`Payload::TestResult`] note for the full
    /// untagged-ordering rationale.
    StatusUpdate {
        /// Id of the node whose status changes.
        #[serde(rename = "nodeId")]
        node_id: String,
        /// The node's new live status.
        status: NodeStatus,
        /// Originating session id.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Id of the agent that produced the update, when attributed.
        #[serde(rename = "agentId", skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        /// OS process id of the producer, when known.
        #[serde(rename = "processId", skip_serializing_if = "Option::is_none")]
        process_id: Option<u32>,
    },
    /// `hot_edge` — a runtime call edge going hot or cold (`DATA_MODEL.md` §A.5).
    ///
    /// Declared **after** [`Payload::StatusUpdate`]: under this `#[serde(untagged)]`
    /// enum a variant resolves by its required fields, and `hot_edge` is the only
    /// variant requiring both `edgeId` and `state`. A `hot_edge` object carries no
    /// `nodeId`/`testId`, so it cannot mis-match [`Payload::TestResult`] or
    /// [`Payload::StatusUpdate`]; conversely a status/test object carries no
    /// `edgeId`, so it falls through this variant. The required `edgeId` is the
    /// disambiguator. Per §A.5, [`HotEdgeState::Enter`] sets the target edge's
    /// [`Edge::hot`] flag and [`HotEdgeState::Exit`] clears it.
    HotEdge {
        /// Id of the call [`Edge`] whose `hot` flag this event toggles.
        #[serde(rename = "edgeId")]
        edge_id: String,
        /// Whether the edge entered (hot) or exited (cold) the executing stack;
        /// its presence (with `edgeId`) disambiguates this variant under the
        /// untagged decode.
        state: HotEdgeState,
        /// OS process id of the producer, when known.
        #[serde(rename = "processId", skip_serializing_if = "Option::is_none")]
        process_id: Option<u32>,
        /// Originating session id.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Id of the agent that produced the event, when attributed.
        #[serde(rename = "agentId", skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        /// ISO-8601 timestamp of the transition.
        ts: String,
    },
    /// `agent.activity` — one tracked agent touched a node (`DATA_MODEL.md` §A.5).
    ///
    /// Declared **last** (with [`Payload::AgentRoster`]): under this
    /// `#[serde(untagged)]` enum a variant resolves by its required fields, and
    /// `action` is required here but carried by no earlier variant, so an
    /// `agent.activity` object resolves here and never captures an earlier payload.
    /// The agent's vertex id is built by [`agent_node_id`].
    AgentActivity {
        /// Id of the agent that performed the action.
        #[serde(rename = "agentId")]
        agent_id: String,
        /// What the agent did (e.g. `"modified"`); its presence disambiguates this
        /// variant under the untagged decode.
        action: String,
        /// Id of the node the agent touched.
        #[serde(rename = "nodeId")]
        node_id: String,
        /// Originating session id.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// OS process id of the agent, when known.
        #[serde(rename = "processId", skip_serializing_if = "Option::is_none")]
        process_id: Option<u32>,
        /// ISO-8601 timestamp of the activity, when supplied.
        #[serde(skip_serializing_if = "Option::is_none")]
        ts: Option<String>,
        /// Optional human-readable detail of the activity.
        #[serde(skip_serializing_if = "Option::is_none")]
        msg: Option<String>,
    },
    /// `agent.roster` — the live set of tracked agents in a session
    /// (`DATA_MODEL.md` §A.5).
    ///
    /// Declared **last** (with [`Payload::AgentActivity`]): its required `agents`
    /// array is carried by no earlier variant, so an `agent.roster` object resolves
    /// here and never mis-decodes as [`Payload::Snapshot`]/[`Payload::Subtree`] or
    /// any other variant.
    AgentRoster {
        /// Originating session id.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// One [`AgentInfo`] row per tracked agent in the session.
        agents: Vec<AgentInfo>,
    },
}

/// A single tracked agent in an [`Payload::AgentRoster`] (`DATA_MODEL.md` §A.5).
///
/// Carries the agent's identity (`agentId`/`agentType`), OS `processId`, display
/// `color`, live `status`, and an optional CLV `protocolVersion`. All JSON keys are
/// camelCase, and `protocolVersion` is omitted when absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    /// OS process id of the agent.
    pub process_id: u32,
    /// Stable id of the agent (e.g. `"tdd-green"`).
    pub agent_id: String,
    /// Agent role / kind (e.g. `"implementation"`, `"security"`).
    pub agent_type: String,
    /// Display colour for the agent (CSS hex string).
    pub color: String,
    /// Live status of the agent (e.g. `"active"`, `"inactive"`).
    pub status: String,
    /// CLV protocol version the agent speaks, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
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

/// Builds a deterministic agent node id from its agent id.
///
/// Implements the `agent:<agentId>` form of `DATA_MODEL.md` §A.1, mirroring
/// [`node_id`]/[`edge_id`]; the `agent` token matches [`NodeType::Agent`]'s
/// [`NodeType::id_prefix`], so an agent vertex keeps a stable identity across runs.
///
/// ```
/// use lattice_backend::wire::agent_node_id;
/// assert_eq!(agent_node_id("security-scanner"), "agent:security-scanner");
/// ```
pub fn agent_node_id(agent_id: &str) -> String {
    format!("agent:{agent_id}")
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

    fn test_result_envelope() -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-27T10:32:01.123Z".to_string(),
            session_id: "sess-abc123".to_string(),
            event_type: EventType::TestResult,
            payload: Payload::TestResult {
                node_id: "fn:a.rs:f".to_string(),
                test_id: "auth::test_valid".to_string(),
                outcome: TestOutcome::Fail,
                duration_ms: Some(14),
                session_id: "sess-abc123".to_string(),
                agent_id: Some("tdd-red".to_string()),
                process_id: Some(48213),
                message: Some("expected Ok, got Err".to_string()),
            },
        }
    }

    fn status_update_envelope() -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-27T10:32:01.123Z".to_string(),
            session_id: "sess-abc123".to_string(),
            event_type: EventType::StatusUpdate,
            payload: Payload::StatusUpdate {
                node_id: "fn:a.rs:f".to_string(),
                status: NodeStatus::Failing,
                session_id: "sess-abc123".to_string(),
                agent_id: None,
                process_id: None,
            },
        }
    }

    fn hot_edge_envelope() -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-27T10:32:01.500Z".to_string(),
            session_id: "sess-abc123".to_string(),
            event_type: EventType::HotEdge,
            payload: Payload::HotEdge {
                edge_id: "e:authenticate->verify_token".to_string(),
                state: HotEdgeState::Enter,
                process_id: Some(48213),
                session_id: "sess-abc123".to_string(),
                agent_id: Some("tdd-green".to_string()),
                ts: "2026-06-27T10:32:01.500Z".to_string(),
            },
        }
    }

    #[test]
    fn test_outcome_serializes_snake_case() {
        let cases = [
            (TestOutcome::Pass, "pass"),
            (TestOutcome::Fail, "fail"),
            (TestOutcome::Skip, "skip"),
            (TestOutcome::Running, "running"),
        ];
        for (variant, want) in cases {
            assert_eq!(
                serde_json::to_value(variant).expect("serialize"),
                Value::from(want)
            );
        }
    }

    #[test]
    fn test_result_envelope_round_trips_with_node_id_key() {
        let env = test_result_envelope();
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(
            json.contains("\"type\":\"test.result\""),
            "missing test.result envelope type: {json}"
        );
        assert!(
            json.contains("\"nodeId\":\"fn:a.rs:f\""),
            "missing camelCase nodeId: {json}"
        );
        assert!(
            json.contains("\"outcome\":\"fail\""),
            "missing snake_case outcome: {json}"
        );
        let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(env, back);
        assert_eq!(back.event_type, EventType::TestResult);
    }

    #[test]
    fn status_update_envelope_round_trips_with_node_id_and_status() {
        let env = status_update_envelope();
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(
            json.contains("\"type\":\"status.update\""),
            "missing status.update envelope type: {json}"
        );
        assert!(
            json.contains("\"nodeId\":\"fn:a.rs:f\""),
            "missing camelCase nodeId: {json}"
        );
        assert!(
            json.contains("\"status\":\"failing\""),
            "missing status value: {json}"
        );
        let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(env, back);
        assert_eq!(back.event_type, EventType::StatusUpdate);
    }

    #[test]
    fn hot_edge_state_serializes_lowercase() {
        let cases = [(HotEdgeState::Enter, "enter"), (HotEdgeState::Exit, "exit")];
        for (variant, want) in cases {
            assert_eq!(
                serde_json::to_value(variant).expect("serialize"),
                Value::from(want)
            );
        }
    }

    #[test]
    fn hot_edge_envelope_round_trips_with_edge_id_and_state() {
        let env = hot_edge_envelope();
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(
            json.contains("\"type\":\"hot_edge\""),
            "missing hot_edge envelope type: {json}"
        );
        assert!(
            json.contains("\"edgeId\":\"e:authenticate->verify_token\""),
            "missing camelCase edgeId: {json}"
        );
        assert!(
            json.contains("\"state\":\"enter\""),
            "missing lowercase state: {json}"
        );
        // camelCase / skip idioms must match §A.5 spelling.
        assert!(
            json.contains("\"processId\":48213"),
            "missing camelCase processId: {json}"
        );
        assert!(
            json.contains("\"agentId\":\"tdd-green\""),
            "missing camelCase agentId: {json}"
        );
        assert!(
            !json.contains("\"edge_id\"") && !json.contains("\"process_id\""),
            "snake_case key leaked: {json}"
        );

        // payload.edgeId / payload.state assert against the §A.5 example values.
        let value = serde_json::to_value(&env).expect("serialize value");
        let payload = &value["payload"];
        assert_eq!(
            payload["edgeId"],
            Value::from("e:authenticate->verify_token")
        );
        assert_eq!(payload["state"], Value::from("enter"));

        let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(env, back);
        assert_eq!(back.event_type, EventType::HotEdge);
        assert!(
            matches!(back.payload, Payload::HotEdge { .. }),
            "hot_edge mis-decoded as {:?}",
            back.payload
        );
    }

    #[test]
    fn hot_edge_process_and_agent_ids_are_omitted_when_absent() {
        let env = EventEnvelope {
            v: 1,
            ts: "2026-06-27T10:32:01.500Z".to_string(),
            session_id: "sess-abc123".to_string(),
            event_type: EventType::HotEdge,
            payload: Payload::HotEdge {
                edge_id: "e:a->b".to_string(),
                state: HotEdgeState::Exit,
                process_id: None,
                session_id: "sess-abc123".to_string(),
                agent_id: None,
                ts: "2026-06-27T10:32:01.500Z".to_string(),
            },
        };
        let value = serde_json::to_value(&env).expect("serialize");
        let payload = value["payload"].as_object().expect("payload object");
        assert!(
            !payload.contains_key("processId"),
            "processId should be omitted when None: {value}"
        );
        assert!(
            !payload.contains_key("agentId"),
            "agentId should be omitted when None: {value}"
        );
        let back: EventEnvelope = serde_json::from_str(&value.to_string()).expect("deserialize");
        assert_eq!(env, back);
    }

    #[test]
    fn untagged_decode_disambiguates_hot_edge_from_status_and_test() {
        // A `hot_edge` payload object resolves to Payload::HotEdge via its required
        // `edgeId` + `state` fields — it must NOT mis-decode as anything else.
        let he: Payload = serde_json::from_str(
            r#"{"edgeId":"e:a->b","state":"enter","sessionId":"s1","ts":"2026-06-27T10:32:01.500Z"}"#,
        )
        .expect("decode hot_edge payload object");
        match he {
            Payload::HotEdge { edge_id, state, .. } => {
                assert_eq!(edge_id, "e:a->b");
                assert_eq!(state, HotEdgeState::Enter);
            }
            other => panic!("hot_edge mis-decoded as {other:?}"),
        }

        // A `status.update` object (no `edgeId`) must still resolve to StatusUpdate.
        let su: Payload =
            serde_json::from_str(r#"{"nodeId":"fn:a.rs:f","status":"passing","sessionId":"s1"}"#)
                .expect("decode status.update payload object");
        assert!(
            matches!(su, Payload::StatusUpdate { .. }),
            "status.update mis-decoded as {su:?}"
        );

        // A `test.result` object (no `edgeId`) must still resolve to TestResult.
        let tr: Payload = serde_json::from_str(
            r#"{"nodeId":"fn:a.rs:f","testId":"t1","outcome":"fail","sessionId":"s1"}"#,
        )
        .expect("decode test.result payload object");
        assert!(
            matches!(tr, Payload::TestResult { .. }),
            "test.result mis-decoded as {tr:?}"
        );
    }

    #[test]
    fn untagged_decode_disambiguates_test_result_and_status_update() {
        // A `test.result` payload object resolves to Payload::TestResult with its
        // `testId` intact — it must NOT mis-decode as a status update.
        let tr: Payload = serde_json::from_str(
            r#"{"nodeId":"fn:a.rs:f","testId":"t1","outcome":"fail","sessionId":"s1"}"#,
        )
        .expect("decode test.result payload object");
        match tr {
            Payload::TestResult {
                node_id,
                test_id,
                outcome,
                ..
            } => {
                assert_eq!(node_id, "fn:a.rs:f");
                assert_eq!(test_id, "t1");
                assert_eq!(outcome, TestOutcome::Fail);
            }
            other => panic!("test.result mis-decoded as {other:?}"),
        }

        // A `status.update` payload object (no `testId`) falls through to
        // Payload::StatusUpdate.
        let su: Payload =
            serde_json::from_str(r#"{"nodeId":"fn:a.rs:f","status":"passing","sessionId":"s1"}"#)
                .expect("decode status.update payload object");
        match su {
            Payload::StatusUpdate {
                node_id, status, ..
            } => {
                assert_eq!(node_id, "fn:a.rs:f");
                assert_eq!(status, NodeStatus::Passing);
            }
            other => panic!("status.update mis-decoded as {other:?}"),
        }

        // A bare `{ "id": … }` still decodes to Payload::NodeRemove (unchanged).
        let rm: Payload =
            serde_json::from_str(r#"{"id":"fn:src/x.rs:foo"}"#).expect("decode bare id object");
        assert!(
            matches!(rm, Payload::NodeRemove { .. }),
            "bare id mis-decoded as {rm:?}"
        );
    }

    // ---- P8-1: agent wire payloads + agent_node_id (DATA_MODEL §A.5) ----
    //
    // RED-phase contract for the agent layer. These tests fix the field names,
    // serde camelCase keys, untagged-decode behaviour, and the `agent:<id>` node-id
    // helper that P8-1 must implement in `wire.rs`. They reference
    // `Payload::AgentActivity`, `Payload::AgentRoster`, the `AgentInfo` struct, and
    // `agent_node_id`, none of which exist yet — so the crate fails to compile until
    // P8-1 adds them.

    /// The literal `agent.activity` payload object from `DATA_MODEL.md` §A.5.
    const AGENT_ACTIVITY_A5_JSON: &str = r##"{"agentId":"security-scanner","action":"modified","nodeId":"fn:src/auth/token.rs:verify_token","sessionId":"sess-abc123","processId":48590}"##;

    /// The literal `agent.roster` payload object from `DATA_MODEL.md` §A.5.
    const AGENT_ROSTER_A5_JSON: &str = r##"{"sessionId":"sess-abc123","agents":[{"processId":48213,"agentId":"tdd-green","agentType":"implementation","color":"#2ecc71","status":"active"},{"processId":48590,"agentId":"security-scanner","agentType":"security","color":"#e67e22","status":"inactive"}]}"##;

    /// Builds the §A.5 `agent.roster` envelope with both agents' optional
    /// `protocolVersion` absent (so the wire object matches the §A.5 literal).
    fn agent_roster_envelope() -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-27T10:32:01.123Z".to_string(),
            session_id: "sess-abc123".to_string(),
            event_type: EventType::AgentRoster,
            payload: Payload::AgentRoster {
                session_id: "sess-abc123".to_string(),
                agents: vec![
                    AgentInfo {
                        process_id: 48213,
                        agent_id: "tdd-green".to_string(),
                        agent_type: "implementation".to_string(),
                        color: "#2ecc71".to_string(),
                        status: "active".to_string(),
                        protocol_version: None,
                    },
                    AgentInfo {
                        process_id: 48590,
                        agent_id: "security-scanner".to_string(),
                        agent_type: "security".to_string(),
                        color: "#e67e22".to_string(),
                        status: "inactive".to_string(),
                        protocol_version: None,
                    },
                ],
            },
        }
    }

    #[test]
    fn agent_activity_decodes_from_data_model_a5_literal() {
        let payload: Payload = serde_json::from_str(AGENT_ACTIVITY_A5_JSON)
            .expect("decode §A.5 agent.activity payload object");
        match payload {
            Payload::AgentActivity {
                agent_id,
                action,
                node_id,
                session_id,
                process_id,
                ts,
                msg,
            } => {
                assert_eq!(agent_id, "security-scanner");
                assert_eq!(action, "modified");
                assert_eq!(node_id, "fn:src/auth/token.rs:verify_token");
                assert_eq!(session_id, "sess-abc123");
                assert_eq!(process_id, Some(48590));
                assert_eq!(ts, None, "ts is absent in the §A.5 example");
                assert_eq!(msg, None, "msg is absent in the §A.5 example");
            }
            other => panic!("agent.activity mis-decoded as {other:?}"),
        }
    }

    #[test]
    fn agent_activity_serializes_field_set_identical_to_a5_literal() {
        // With the optionals (ts/msg) None, skip_serializing_if must omit them and
        // the serialized object must match the §A.5 literal field-for-field.
        let payload: Payload = serde_json::from_str(AGENT_ACTIVITY_A5_JSON)
            .expect("decode §A.5 agent.activity payload object");
        let got = serde_json::to_value(&payload).expect("serialize");
        let want: Value =
            serde_json::from_str(AGENT_ACTIVITY_A5_JSON).expect("parse §A.5 literal to a Value");
        assert_eq!(got, want, "field set must match §A.5 literal");

        let obj = got.as_object().expect("payload is a JSON object");
        assert_eq!(
            obj.len(),
            5,
            "exactly the 5 §A.5 keys, no optionals leaked: {got}"
        );
        assert!(
            !obj.contains_key("ts"),
            "ts must be omitted when None: {got}"
        );
        assert!(
            !obj.contains_key("msg"),
            "msg must be omitted when None: {got}"
        );
        assert!(
            obj.contains_key("processId") && !obj.contains_key("process_id"),
            "processId must be camelCase, not snake_case: {got}"
        );
        assert!(
            obj.contains_key("agentId")
                && obj.contains_key("nodeId")
                && obj.contains_key("sessionId"),
            "agentId/nodeId/sessionId must be camelCase: {got}"
        );
    }

    #[test]
    fn agent_activity_envelope_round_trips_with_optionals_present() {
        // Full envelope round-trip with every optional populated, including ts/msg.
        let env = EventEnvelope {
            v: 1,
            ts: "2026-06-27T10:32:01.123Z".to_string(),
            session_id: "sess-abc123".to_string(),
            event_type: EventType::AgentActivity,
            payload: Payload::AgentActivity {
                agent_id: "tdd-red".to_string(),
                action: "modified".to_string(),
                node_id: "fn:src/wire.rs:agent_node_id".to_string(),
                session_id: "sess-abc123".to_string(),
                process_id: Some(48590),
                ts: Some("2026-06-27T10:32:01.123Z".to_string()),
                msg: Some("wrote failing tests".to_string()),
            },
        };
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(
            json.contains("\"type\":\"agent.activity\""),
            "missing agent.activity envelope type: {json}"
        );
        assert!(
            json.contains("\"agentId\":\"tdd-red\""),
            "missing camelCase agentId: {json}"
        );
        let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(env, back);
        assert_eq!(back.event_type, EventType::AgentActivity);
        assert!(
            matches!(back.payload, Payload::AgentActivity { .. }),
            "agent.activity mis-decoded as {:?}",
            back.payload
        );
    }

    #[test]
    fn agent_roster_decodes_from_data_model_a5_literal() {
        let payload: Payload = serde_json::from_str(AGENT_ROSTER_A5_JSON)
            .expect("decode §A.5 agent.roster payload object");
        match payload {
            Payload::AgentRoster { session_id, agents } => {
                assert_eq!(session_id, "sess-abc123");
                assert_eq!(agents.len(), 2);
                assert_eq!(
                    agents[0],
                    AgentInfo {
                        process_id: 48213,
                        agent_id: "tdd-green".to_string(),
                        agent_type: "implementation".to_string(),
                        color: "#2ecc71".to_string(),
                        status: "active".to_string(),
                        protocol_version: None,
                    }
                );
                assert_eq!(
                    agents[1],
                    AgentInfo {
                        process_id: 48590,
                        agent_id: "security-scanner".to_string(),
                        agent_type: "security".to_string(),
                        color: "#e67e22".to_string(),
                        status: "inactive".to_string(),
                        protocol_version: None,
                    }
                );
            }
            other => panic!("agent.roster mis-decoded as {other:?}"),
        }
    }

    #[test]
    fn agent_info_serializes_camel_case_and_omits_absent_protocol_version() {
        let info = AgentInfo {
            process_id: 48213,
            agent_id: "tdd-green".to_string(),
            agent_type: "implementation".to_string(),
            color: "#2ecc71".to_string(),
            status: "active".to_string(),
            protocol_version: None,
        };
        let value = serde_json::to_value(&info).expect("serialize AgentInfo");
        let obj = value.as_object().expect("AgentInfo is a JSON object");
        for key in ["processId", "agentId", "agentType", "color", "status"] {
            assert!(
                obj.contains_key(key),
                "missing camelCase key {key}: {value}"
            );
        }
        assert!(
            !obj.contains_key("protocolVersion"),
            "protocolVersion must be omitted when None: {value}"
        );
        assert!(
            !obj.contains_key("process_id") && !obj.contains_key("agent_type"),
            "snake_case key leaked: {value}"
        );

        // When present, protocolVersion serializes under its camelCase key.
        let info2 = AgentInfo {
            protocol_version: Some("1".to_string()),
            ..info
        };
        let value2 = serde_json::to_value(&info2).expect("serialize AgentInfo");
        assert_eq!(value2["protocolVersion"], Value::from("1"));
    }

    #[test]
    fn agent_roster_envelope_round_trips() {
        let env = agent_roster_envelope();
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(
            json.contains("\"type\":\"agent.roster\""),
            "missing agent.roster envelope type: {json}"
        );
        assert!(
            json.contains("\"agentType\":\"implementation\""),
            "missing camelCase agentType in nested AgentInfo: {json}"
        );
        let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(env, back);
        assert_eq!(back.event_type, EventType::AgentRoster);
        assert!(
            matches!(back.payload, Payload::AgentRoster { .. }),
            "agent.roster mis-decoded as {:?}",
            back.payload
        );
    }

    #[test]
    fn untagged_decode_disambiguates_agent_payloads() {
        // The `Payload` enum is `#[serde(untagged)]` with order-sensitive decode. An
        // agent.activity object must resolve to Payload::AgentActivity, NOT to an
        // earlier variant (it carries `agentId`+`action`, which no earlier variant
        // requires).
        let activity: Payload = serde_json::from_str(AGENT_ACTIVITY_A5_JSON)
            .expect("decode agent.activity payload object");
        assert!(
            matches!(activity, Payload::AgentActivity { .. }),
            "agent.activity mis-decoded as {activity:?}"
        );

        // An agent.roster object (`sessionId`+`agents`) must resolve to
        // Payload::AgentRoster, NOT to Payload::Snapshot/Subtree or any other variant.
        let roster: Payload =
            serde_json::from_str(AGENT_ROSTER_A5_JSON).expect("decode agent.roster payload object");
        assert!(
            matches!(roster, Payload::AgentRoster { .. }),
            "agent.roster mis-decoded as {roster:?}"
        );
    }

    #[test]
    fn agent_node_id_uses_agent_prefix() {
        let cases = [
            ("security-scanner", "agent:security-scanner"),
            ("tdd-green", "agent:tdd-green"),
            ("", "agent:"),
        ];
        for (input, want) in cases {
            assert_eq!(agent_node_id(input), want, "input: {input:?}");
        }
    }

    #[test]
    fn agent_node_id_is_deterministic() {
        assert_eq!(
            agent_node_id("security-scanner"),
            agent_node_id("security-scanner"),
            "agent_node_id must be a pure function of its input"
        );
    }

    #[test]
    fn agent_node_id_matches_agent_type_id_prefix() {
        // The `agent` token mirrors NodeType::Agent's id prefix (the §A.1 convention).
        assert!(
            agent_node_id("x").starts_with(&format!("{}:", NodeType::Agent.id_prefix())),
            "agent_node_id must start with the NodeType::Agent id prefix"
        );
    }

    #[test]
    fn protocol_sentinel_unchanged_by_phase_8() {
        // P8-1 must not bump the CLV sentinel.
        assert_eq!(crate::protocol_sentinel(), "#CLV1");
    }
}
