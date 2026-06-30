//! In-memory CLV graph and the diff that turns a re-parse into patch events.
//!
//! [`Graph`] is the Phase-0 source of truth for the live structural graph: it
//! holds the current [`Node`](crate::wire::Node)s and
//! [`Edge`](crate::wire::Edge)s keyed by their deterministic ids and turns
//! parser output into the [`EventEnvelope`](crate::wire::EventEnvelope) stream a
//! WebSocket client consumes.
//!
//! Three write paths exist. [`Graph::upsert_node`]/[`Graph::upsert_edge`] are raw
//! insert-or-update-by-id mutators. [`Graph::apply_parsed`] is the higher-level
//! structural path: it diffs a file's previous contribution against a fresh
//! [`ParsedFile`](crate::parser::ParsedFile) and emits `node.upsert`/`edge.upsert`
//! for added-or-changed elements and `node.remove`/`edge.remove` for elements that
//! vanished from that file, so re-applying an identical parse is a no-op.
//! [`Graph::apply_clv`] is the live-overlay path and returns a `Vec` of the patch
//! envelopes one event produces: it maps a correlated
//! [`ClvEvent`](crate::clv::ClvEvent) `test`/`status` event onto the target node's
//! [`NodeStatus`](crate::wire::NodeStatus) and emits the matching
//! `test.result`/`status.update` envelope (Phase 5), and a `hotedge` `enter`/`exit`
//! event onto the target [`Edge::hot`](crate::wire::Edge) flag, emitting a
//! transition-coalesced `hot_edge` envelope (Phase 6). An `activity` event carrying
//! an `agent` id and `pid` drives the **Phase-8 agent layer**: it upserts a root
//! `agent` vertex and an `authored_by` edge from the touched code node, updates the
//! per-process [`AgentInfo`](crate::wire::AgentInfo) roster, and emits
//! `node.upsert`/`edge.upsert`/`agent.activity`/`agent.roster` — though the
//! `agent.roster` is coalesced away on a no-change repeat (same pid, same identity).
//! An unknown node/edge id, an unparsable `hotedge` state, a no-change heat
//! transition, or an `activity` event missing its `agent`/`pid` yields an empty
//! `Vec` (a no-op).
//!
//! Reads are lazy (Phase 1). [`Graph::snapshot`] renders only the **root** tier
//! (file nodes and the edges among them) for a freshly connected client; deeper
//! tiers load on demand: [`Graph::children_of`] returns a node's direct children,
//! and [`Graph::subtree`] wraps them in a `subtree` envelope replying to an
//! `expand` request.
//!
//! Per `AGENT_PROTOCOL.md` §6 this is panic-free: a clock error or a parse with no
//! `file` node degrades to a safe default rather than unwrapping.

use std::collections::{HashMap, HashSet};

use crate::clv::ClvEvent;
use crate::parser::ParsedFile;
use crate::wire::{
    agent_node_id, typed_edge_id, AgentInfo, Edge, EdgeKind, EventEnvelope, EventType,
    HotEdgeState, Node, NodeStatus, NodeType, Payload, TestOutcome,
};

/// CLV protocol version stamped on every envelope this graph emits.
const PROTOCOL_VERSION: u32 = 1;

/// Session id used when a [`Graph`] is created without an explicit one.
const DEFAULT_SESSION_ID: &str = "sess-local";

/// The in-memory structural graph and the diff that emits CLV patch events.
///
/// Holds the current [`Node`]s and [`Edge`]s keyed by their deterministic ids,
/// plus the set of ids each source file last contributed (keyed by that file's
/// `file:<path>` node id) so [`Graph::apply_parsed`] can compute removals. The
/// `session_id` is stamped onto every emitted [`EventEnvelope`].
#[derive(Debug, Clone)]
pub struct Graph {
    /// Session id stamped onto every emitted envelope.
    session_id: String,
    /// Current nodes, keyed by [`Node::id`].
    nodes: HashMap<String, Node>,
    /// Current edges, keyed by [`Edge::id`].
    edges: HashMap<String, Edge>,
    /// Node ids each file last contributed, keyed by the file node id.
    file_nodes: HashMap<String, Vec<String>>,
    /// Edge ids each file last contributed, keyed by the file node id.
    file_edges: HashMap<String, Vec<String>>,
    /// Live agent roster (Phase 8), keyed by OS **process id**.
    ///
    /// Each [`AgentInfo`] row records one tracked process's identity (agent id,
    /// type, colour) and live `status`. Keyed by `process_id` because one agent id
    /// may run as several concurrent processes, each a distinct roster row. An
    /// `activity` event upserts the row for its pid, and [`Graph::apply_clv`]
    /// snapshots every row into the emitted `agent.roster` envelope.
    roster: HashMap<u32, AgentInfo>,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    /// Creates an empty graph using the default session id (`"sess-local"`).
    pub fn new() -> Self {
        Self::with_session(DEFAULT_SESSION_ID)
    }

    /// Creates an empty graph that stamps `session_id` onto every emitted envelope.
    pub fn with_session(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            nodes: HashMap::new(),
            edges: HashMap::new(),
            file_nodes: HashMap::new(),
            file_edges: HashMap::new(),
            roster: HashMap::new(),
        }
    }

    /// Inserts `node`, or replaces the existing node with the same [`Node::id`].
    pub fn upsert_node(&mut self, node: Node) {
        self.nodes.insert(node.id.clone(), node);
    }

    /// Inserts `edge`, or replaces the existing edge with the same [`Edge::id`].
    pub fn upsert_edge(&mut self, edge: Edge) {
        self.edges.insert(edge.id.clone(), edge);
    }

    /// Renders the top-level (root-only) graph as one `snapshot` [`EventEnvelope`].
    ///
    /// The payload is [`Payload::Snapshot`] carrying only **root** nodes (those
    /// with no `parent_id`, i.e. files) and the edges whose source and target are
    /// both roots; child tiers (functions, variables) are omitted and load lazily
    /// via [`Graph::subtree`]. The WebSocket server sends this to each client on
    /// connect and on resync.
    pub fn snapshot(&self) -> EventEnvelope {
        let root_ids: HashSet<&str> = self
            .nodes
            .values()
            .filter(|n| n.parent_id.is_none())
            .map(|n| n.id.as_str())
            .collect();
        let nodes = self
            .nodes
            .values()
            .filter(|n| n.parent_id.is_none())
            .cloned()
            .collect();
        let edges = self
            .edges
            .values()
            .filter(|e| {
                root_ids.contains(e.source.as_str()) && root_ids.contains(e.target.as_str())
            })
            .cloned()
            .collect();
        self.envelope(EventType::Snapshot, Payload::Snapshot { nodes, edges })
    }

    /// Returns a node's **direct** children and the `contains` edges to them.
    ///
    /// Selects every node whose `parent_id` is exactly `node_id` (one tier down,
    /// never grandchildren) and the `contains` [`Edge`]s from `node_id` to those
    /// children. Backs the lazy-hierarchy `expand` flow: expanding a `file` node
    /// yields its `function` children, expanding a `function` yields its
    /// `variable` children. Returns empty vectors when `node_id` is unknown or
    /// childless.
    pub fn children_of(&self, node_id: &str) -> (Vec<Node>, Vec<Edge>) {
        let nodes: Vec<Node> = self
            .nodes
            .values()
            .filter(|n| n.parent_id.as_deref() == Some(node_id))
            .cloned()
            .collect();
        let child_ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let edges: Vec<Edge> = self
            .edges
            .values()
            .filter(|e| {
                e.kind == EdgeKind::Contains
                    && e.source == node_id
                    && child_ids.contains(e.target.as_str())
            })
            .cloned()
            .collect();
        (nodes, edges)
    }

    /// Renders a node's direct children as one `subtree` [`EventEnvelope`].
    ///
    /// Wraps [`Graph::children_of`] in a [`Payload::Subtree`] stamped (via the
    /// private [`Graph::envelope`] helper) with this graph's session id and
    /// protocol version — exactly like [`Graph::snapshot`] — so the WebSocket
    /// server can reply to a client `expand` request without reaching into the
    /// graph's private session state. The payload's `parent_id` echoes `node_id`.
    pub fn subtree(&self, node_id: &str) -> EventEnvelope {
        let (nodes, edges) = self.children_of(node_id);
        self.envelope(
            EventType::Subtree,
            Payload::Subtree {
                parent_id: node_id.to_string(),
                nodes,
                edges,
            },
        )
    }

    /// Applies a freshly parsed file and returns the patch events it produces.
    ///
    /// Diffs `parsed` against the file's previous contribution (located by the
    /// `file:<path>` node in `parsed.nodes`):
    /// - every node/edge that is new or no longer byte-equal to the stored one
    ///   is upserted and yields a `node.upsert`/`edge.upsert` envelope;
    /// - every id the file previously contributed but no longer does is removed
    ///   and yields a `node.remove`/`edge.remove` envelope.
    ///
    /// Re-applying an identical [`ParsedFile`] mutates nothing and returns an
    /// empty vector. If `parsed` carries no `file` node there is nothing to
    /// anchor the diff against, so an empty vector is returned (panic-free).
    pub fn apply_parsed(&mut self, parsed: ParsedFile) -> Vec<EventEnvelope> {
        let ParsedFile { nodes, edges } = parsed;

        let file_key = match nodes.iter().find(|n| n.node_type == NodeType::File) {
            Some(file) => file.id.clone(),
            None => return Vec::new(),
        };

        let new_node_ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
        let new_edge_ids: Vec<String> = edges.iter().map(|e| e.id.clone()).collect();
        let mut events = Vec::new();

        for node in nodes {
            if self.nodes.get(&node.id) != Some(&node) {
                self.nodes.insert(node.id.clone(), node.clone());
                events.push(self.envelope(EventType::NodeUpsert, Payload::NodeUpsert { node }));
            }
        }
        for edge in edges {
            if self.edges.get(&edge.id) != Some(&edge) {
                self.edges.insert(edge.id.clone(), edge.clone());
                events.push(self.envelope(EventType::EdgeUpsert, Payload::EdgeUpsert { edge }));
            }
        }

        let prev_nodes = self.file_nodes.get(&file_key).cloned().unwrap_or_default();
        for id in prev_nodes {
            if !new_node_ids.contains(&id) {
                self.nodes.remove(&id);
                events.push(self.envelope(EventType::NodeRemove, Payload::NodeRemove { id }));
            }
        }
        let prev_edges = self.file_edges.get(&file_key).cloned().unwrap_or_default();
        for id in prev_edges {
            if !new_edge_ids.contains(&id) {
                self.edges.remove(&id);
                events.push(self.envelope(EventType::EdgeRemove, Payload::EdgeRemove { id }));
            }
        }

        self.file_nodes.insert(file_key.clone(), new_node_ids);
        self.file_edges.insert(file_key, new_edge_ids);

        events
    }

    /// Applies one correlated CLV event and returns every patch it produces.
    ///
    /// Maps an [`AGENT_PROTOCOL.md` §2](crate::clv) [`ClvEvent`] onto the live graph,
    /// returning a [`Vec`] of [`EventEnvelope`]s for the WebSocket layer to broadcast.
    /// A `Test`/`Status`/`HotEdge` event yields a **one-element** vector (or an empty
    /// one — see below); an `activity` event yields **several** envelopes (the
    /// Phase-8 agent layer), which is why the return widened from a single
    /// `Option<EventEnvelope>` to a vector.
    /// - [`ClvEvent::Test`] / [`ClvEvent::Status`]: when the event's `node` id is a
    ///   known node, its stored [`Node::status`] is set from the event `outcome`
    ///   ([`TestOutcome::Fail`]→`Failing`, [`TestOutcome::Pass`]→`Passing`,
    ///   [`TestOutcome::Skip`]→`Stale`, [`TestOutcome::Running`]→`Running`) so a later
    ///   [`Graph::snapshot`]/[`Graph::subtree`] reflects the colour, and the method
    ///   returns a one-element vector holding a `test.result` / `status.update`
    ///   [`EventEnvelope`] (stamped via [`Graph::envelope`], exactly like
    ///   [`Graph::apply_parsed`]).
    /// - [`ClvEvent::HotEdge`] (Phase 6): when the event's `edge` id is a known edge
    ///   and the `state` word parses (`enter`→hot, `exit`→cold; any other string is a
    ///   no-op), the stored [`Edge::hot`] flag is toggled and the method returns a
    ///   one-element vector holding a `hot_edge` [`EventEnvelope`] carrying the
    ///   matching [`HotEdgeState`](crate::wire::HotEdgeState). **Transition-coalescing:**
    ///   if the edge is already in the target heat the call returns an empty vector and
    ///   emits nothing, so a hot loop re-entering an already-hot edge cannot flood
    ///   clients. A hot-edge event never touches any [`Node::status`].
    /// - [`ClvEvent::Activity`] (Phase 8 agent layer): when the event carries **both**
    ///   an `agent` id and a `pid`, it attributes the touch to that agent — see
    ///   [`Graph::apply_activity`] — upserting a root [`NodeType::Agent`] vertex
    ///   (id via [`agent_node_id`], reused on repeat so there is no duplicate), an
    ///   [`EdgeKind::AuthoredBy`] [`Edge`] from the touched code node to that vertex
    ///   (deterministic, kind-qualified id via [`typed_edge_id`]), and the per-pid
    ///   [`AgentInfo`] roster row (`status: "active"`). It returns up to four
    ///   envelopes: `node.upsert` (agent), `edge.upsert` (authored_by),
    ///   `agent.activity`, and `agent.roster`. **Roster-coalescing** (SPEC §11.2):
    ///   the `agent.roster` is emitted only when this pid's row was newly inserted or
    ///   actually changed, so a steady-state re-touch (same pid, same identity) skips
    ///   the roster rebuild/broadcast and emits only `edge.upsert` + `agent.activity`.
    ///   An `activity` event missing its `agent` id or `pid` cannot be attributed and
    ///   is a no-op (empty vector); a code node's colour is never changed by an
    ///   activity.
    /// - **Absent target:** a `Test`/`Status` event whose `node` id — or a `HotEdge`
    ///   event whose `edge` id — is *not* in the graph returns an empty vector and
    ///   mutates nothing (the emitter-↔-graph id contract; an out-of-graph id is
    ///   ignored, never an error).
    ///
    /// [`Node::status`], [`Edge::hot`], the agent vertex/edge, and the [`Graph`]'s
    /// `roster` are the only state touched; the file-contribution bookkeeping
    /// ([`Graph::apply_parsed`] relies on) is left intact, so colouring a node,
    /// heating an edge, or attributing an activity then re-parsing its source keeps
    /// the structural diff correct. Panic-free.
    pub fn apply_clv(&mut self, event: &ClvEvent) -> Vec<EventEnvelope> {
        match event {
            ClvEvent::Test {
                node,
                outcome,
                session,
                pid,
                agent,
                msg,
                duration_ms,
            } => {
                let status = status_from_outcome(*outcome);
                // Absent node id is ignored (the emitter-↔-graph id contract).
                match self.nodes.get_mut(node) {
                    Some(target) => target.status = status,
                    None => return Vec::new(),
                }
                vec![self.envelope(
                    EventType::TestResult,
                    Payload::TestResult {
                        node_id: node.clone(),
                        test_id: node.clone(),
                        outcome: *outcome,
                        duration_ms: *duration_ms,
                        session_id: session.clone(),
                        agent_id: agent.clone(),
                        process_id: *pid,
                        message: msg.clone(),
                    },
                )]
            }
            ClvEvent::Status {
                node,
                outcome,
                session,
                pid,
                agent,
                ..
            } => {
                let status = status_from_outcome(*outcome);
                match self.nodes.get_mut(node) {
                    Some(target) => target.status = status,
                    None => return Vec::new(),
                }
                vec![self.envelope(
                    EventType::StatusUpdate,
                    Payload::StatusUpdate {
                        node_id: node.clone(),
                        status,
                        session_id: session.clone(),
                        agent_id: agent.clone(),
                        process_id: *pid,
                    },
                )]
            }
            ClvEvent::Activity {
                session,
                pid,
                agent,
                msg,
                node,
                action,
            } => self.apply_activity(
                session,
                *pid,
                agent.as_deref(),
                msg.as_deref(),
                node,
                action,
            ),
            ClvEvent::HotEdge {
                edge,
                state,
                session,
                pid,
                agent,
                ..
            } => {
                // Parse the free-string transition; any word but enter/exit is a
                // panic-free no-op.
                let (target, target_hot) = match state.as_str() {
                    "enter" => (HotEdgeState::Enter, true),
                    "exit" => (HotEdgeState::Exit, false),
                    _ => return Vec::new(),
                };
                // Absent edge id is ignored (mirrors the absent-node contract).
                let stored = match self.edges.get_mut(edge) {
                    Some(stored) => stored,
                    None => return Vec::new(),
                };
                // Transition-coalescing: a no-change transition emits nothing, so a
                // hot loop re-entering an already-hot edge does not flood clients.
                if stored.hot == target_hot {
                    return Vec::new();
                }
                stored.hot = target_hot;
                // The mutable edge borrow ends here; build the envelope from the
                // event's own fields so `&self` can be borrowed afresh. Mint the
                // timestamp once and share it between the payload and the envelope
                // so a transition takes a single clock reading inside the Mutex.
                let ts = rfc3339_now();
                vec![self.envelope_at(
                    ts.clone(),
                    EventType::HotEdge,
                    Payload::HotEdge {
                        edge_id: edge.clone(),
                        state: target,
                        process_id: *pid,
                        session_id: session.clone(),
                        agent_id: agent.clone(),
                        ts,
                    },
                )]
            }
        }
    }

    /// Attributes one `activity` touch to its agent, returning the four patches.
    ///
    /// The Phase-8 agent-layer body of [`Graph::apply_clv`]'s [`ClvEvent::Activity`]
    /// arm. Attribution needs both an `agent` id (the vertex identity) and a `pid`
    /// (the roster key); if either is absent the touch cannot be attributed, so the
    /// method mutates nothing and returns an empty vector. Otherwise it:
    /// 1. upserts a root [`NodeType::Agent`] vertex (id [`agent_node_id`], label the
    ///    agent id, `parent_id` `None`), reusing the id on repeat so there is no
    ///    duplicate node and a `node.upsert` is emitted only the first time;
    /// 2. upserts an [`EdgeKind::AuthoredBy`] [`Edge`] from the touched code `node` to
    ///    the agent vertex, with the deterministic, kind-qualified [`typed_edge_id`]
    ///    so re-touches keep one stable edge id;
    /// 3. records/refreshes the per-pid [`AgentInfo`] roster row as `"active"`,
    ///    deriving a stable `color` and `agent_type` from the agent id — but
    ///    **coalesces** the high-frequency case (SPEC §11.2): when this pid is already
    ///    rostered with an identical row, the roster is left untouched and neither the
    ///    `color`/`agent_type` derivation nor the O(N) roster clone+sort runs; and
    /// 4. returns `node.upsert`, `edge.upsert`, `agent.activity`, and `agent.roster`
    ///    envelopes. The `node.upsert` is omitted when the agent vertex already
    ///    existed, and the `agent.roster` is omitted on a no-change roster repeat
    ///    (same pid, same identity), so a steady-state re-touch emits only the
    ///    `edge.upsert` and `agent.activity`.
    ///
    /// The touched code node's [`Node::status`] is never changed. Panic-free.
    fn apply_activity(
        &mut self,
        session: &str,
        pid: Option<u32>,
        agent: Option<&str>,
        msg: Option<&str>,
        node: &str,
        action: &str,
    ) -> Vec<EventEnvelope> {
        let (agent_id, pid) = match (agent, pid) {
            (Some(agent_id), Some(pid)) => (agent_id, pid),
            _ => return Vec::new(),
        };
        let agent_vertex = agent_node_id(agent_id);
        let mut events = Vec::new();

        // (1) Upsert the agent vertex once; reuse its id on repeat.
        if !self.nodes.contains_key(&agent_vertex) {
            let agent_node = Node {
                id: agent_vertex.clone(),
                node_type: NodeType::Agent,
                label: agent_id.to_string(),
                parent_id: None,
                child_ids: Vec::new(),
                status: NodeStatus::Unknown,
                docs: None,
                signature: None,
                meta: None,
            };
            self.nodes.insert(agent_vertex.clone(), agent_node.clone());
            events.push(self.envelope(
                EventType::NodeUpsert,
                Payload::NodeUpsert { node: agent_node },
            ));
        }

        // (2) Upsert the authored_by edge (touched code node → agent vertex) with the
        // kind-qualified id (the house convention for non-`contains` edge kinds).
        let edge = Edge {
            id: typed_edge_id(node, &agent_vertex, EdgeKind::AuthoredBy),
            source: node.to_string(),
            target: agent_vertex,
            kind: EdgeKind::AuthoredBy,
            hot: false,
        };
        self.edges.insert(edge.id.clone(), edge.clone());
        events.push(self.envelope(EventType::EdgeUpsert, Payload::EdgeUpsert { edge }));

        // (3) Record/refresh this process's roster row as active, coalescing the
        // high-frequency case (SPEC §11.2): if this pid is already rostered with an
        // identical row — same agent identity, still "active" — skip the row rebuild,
        // the O(N) roster clone+sort, and the extra broadcast, mirroring the hot-edge
        // no-change early return above. `agent_type`/`color` derive purely from the
        // agent id, so an unchanged `agent_id` (with the same status/protocol) means
        // the whole row is unchanged and need not be recomputed.
        let roster_unchanged = matches!(
            self.roster.get(&pid),
            Some(existing)
                if existing.agent_id == agent_id
                    && existing.status == "active"
                    && existing.protocol_version.is_none()
        );
        if !roster_unchanged {
            self.roster.insert(
                pid,
                AgentInfo {
                    process_id: pid,
                    agent_id: agent_id.to_string(),
                    agent_type: agent_type_for(agent_id),
                    color: agent_color_for(agent_id),
                    status: "active".to_string(),
                    protocol_version: None,
                },
            );
        }

        // (4) Emit the agent.activity envelope, then the agent.roster envelope only
        // when the roster actually changed for this touch.
        events.push(self.envelope(
            EventType::AgentActivity,
            Payload::AgentActivity {
                agent_id: agent_id.to_string(),
                action: action.to_string(),
                node_id: node.to_string(),
                session_id: session.to_string(),
                process_id: Some(pid),
                ts: None,
                msg: msg.map(str::to_string),
            },
        ));
        if !roster_unchanged {
            let mut agents: Vec<AgentInfo> = self.roster.values().cloned().collect();
            agents.sort_by_key(|a| a.process_id);
            events.push(self.envelope(
                EventType::AgentRoster,
                Payload::AgentRoster {
                    session_id: session.to_string(),
                    agents,
                },
            ));
        }

        events
    }

    /// Wraps `payload` in an [`EventEnvelope`] stamped now with this graph's session.
    fn envelope(&self, event_type: EventType, payload: Payload) -> EventEnvelope {
        self.envelope_at(rfc3339_now(), event_type, payload)
    }

    /// Wraps `payload` in an [`EventEnvelope`] stamped with the supplied `ts` and
    /// this graph's session. Lets a caller that has already minted a timestamp —
    /// e.g. the hot-edge arm, whose `Payload::HotEdge` carries its own `ts` — reuse
    /// it instead of taking a second clock reading inside the collector's critical
    /// section.
    fn envelope_at(&self, ts: String, event_type: EventType, payload: Payload) -> EventEnvelope {
        EventEnvelope {
            v: PROTOCOL_VERSION,
            ts,
            session_id: self.session_id.clone(),
            event_type,
            payload,
        }
    }
}

/// Maps a CLV `outcome` word onto the [`NodeStatus`] colour it sets.
///
/// Shared by the [`ClvEvent::Test`] and [`ClvEvent::Status`] arms of
/// [`Graph::apply_clv`]: [`TestOutcome::Fail`]→[`NodeStatus::Failing`],
/// [`TestOutcome::Pass`]→[`NodeStatus::Passing`], [`TestOutcome::Skip`]→
/// [`NodeStatus::Stale`], [`TestOutcome::Running`]→[`NodeStatus::Running`].
fn status_from_outcome(outcome: TestOutcome) -> NodeStatus {
    match outcome {
        TestOutcome::Pass => NodeStatus::Passing,
        TestOutcome::Fail => NodeStatus::Failing,
        TestOutcome::Skip => NodeStatus::Stale,
        TestOutcome::Running => NodeStatus::Running,
    }
}

/// Folds an agent id with FNV-1a into a stable 32-bit hash.
///
/// Shared seed of the deterministic [`agent_type_for`]/[`agent_color_for`]
/// mappings (Phase 8): a given agent id always yields the same hash, so its
/// roster type and colour are stable across processes and runs.
fn agent_id_hash(agent_id: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in agent_id.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Derives a stable display colour (`#rrggbb`) for an agent from its id.
///
/// Deterministic via [`agent_id_hash`]: the low 24 bits of the hash become the RGB
/// triple, so each agent id keeps one visually distinct colour across runs. The
/// roster's `color` is presentation-only (consumed by the Phase-8.3/8.6 client),
/// never an identity, so any stable mapping suffices.
fn agent_color_for(agent_id: &str) -> String {
    format!("#{:06x}", agent_id_hash(agent_id) & 0x00ff_ffff)
}

/// Derives a stable role label for an agent from its id.
///
/// Deterministic (Phase 8): pending a richer agent-role taxonomy, the `agentType`
/// mirrors the stable agent id itself, which is the only role information the wire
/// carries today. The empty id collapses to `"agent"` so the field is never blank.
fn agent_type_for(agent_id: &str) -> String {
    if agent_id.is_empty() {
        "agent".to_string()
    } else {
        agent_id.to_string()
    }
}

/// Returns a best-effort RFC3339 UTC timestamp for an outgoing envelope.
///
/// Phase 0 does not interpret `ts` semantically — clients and tests key only on
/// the envelope `type` and `payload` — so this is a derived stamp that is never
/// asserted on. It is panic-free: a system clock reporting a time before the Unix
/// epoch falls back to the epoch itself. The civil-date conversion is Howard
/// Hinnant's `civil_from_days` algorithm and uses only total integer arithmetic.
fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (hour, minute, second) = (tod / 3_600, (tod % 3_600) / 60, tod % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::Graph;
    use crate::clv::ClvEvent;
    use crate::parser::parse_rust_source;
    use crate::wire::{
        agent_node_id, edge_id, node_id, AgentInfo, Edge, EdgeKind, EventEnvelope, EventType,
        HotEdgeState, Node, NodeStatus, NodeType, Payload, TestOutcome,
    };

    fn function_node(id: &str, label: &str) -> Node {
        Node {
            id: id.to_string(),
            node_type: NodeType::Function,
            label: label.to_string(),
            parent_id: None,
            child_ids: Vec::new(),
            status: NodeStatus::Unknown,
            docs: None,
            signature: None,
            meta: None,
        }
    }

    fn snapshot_nodes(env: &EventEnvelope) -> Vec<Node> {
        match &env.payload {
            Payload::Snapshot { nodes, .. } => nodes.clone(),
            other => panic!("expected snapshot payload, got {other:?}"),
        }
    }

    fn snapshot_edges(env: &EventEnvelope) -> Vec<Edge> {
        match &env.payload {
            Payload::Snapshot { edges, .. } => edges.clone(),
            other => panic!("expected snapshot payload, got {other:?}"),
        }
    }

    fn upserted_node<'a>(events: &'a [EventEnvelope], id: &str) -> Option<&'a Node> {
        events
            .iter()
            .find_map(|env| match (&env.event_type, &env.payload) {
                (EventType::NodeUpsert, Payload::NodeUpsert { node }) if node.id == id => {
                    Some(node)
                }
                _ => None,
            })
    }

    fn removed_node_ids(events: &[EventEnvelope]) -> Vec<String> {
        events
            .iter()
            .filter_map(|env| match (&env.event_type, &env.payload) {
                (EventType::NodeRemove, Payload::NodeRemove { id }) => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn upsert_same_id_twice_keeps_one_node_with_latest_label() {
        let mut graph = Graph::new();
        let id = node_id(NodeType::Function, "src/x.rs", "foo");
        graph.upsert_node(function_node(&id, "old"));
        graph.upsert_node(function_node(&id, "new"));

        let nodes = snapshot_nodes(&graph.snapshot());
        let matching: Vec<&Node> = nodes.iter().filter(|n| n.id == id).collect();
        assert_eq!(matching.len(), 1, "exactly one node for the id: {nodes:?}");
        assert_eq!(matching[0].label, "new", "latest label wins");
    }

    #[test]
    fn snapshot_contains_only_root_nodes_and_edges() {
        let mut graph = Graph::new();
        let _ = graph.apply_parsed(parse_rust_source("src/x.rs", "fn foo() {}"));

        let env = graph.snapshot();
        assert_eq!(env.event_type, EventType::Snapshot);
        let nodes = snapshot_nodes(&env);
        let edges = snapshot_edges(&env);

        // The file node is a root (parentId == None) and is present.
        assert!(
            nodes.iter().any(|n| n.id == "file:src/x.rs"),
            "snapshot must contain the root file node: {nodes:?}"
        );
        // The function is a child of the file, so the lazy top level omits it.
        assert!(
            !nodes.iter().any(|n| n.id == "fn:src/x.rs:foo"),
            "lazy snapshot must NOT contain the child function node: {nodes:?}"
        );
        // The file→function `contains` edge straddles tiers, so it is omitted too.
        let contains = edge_id("file:src/x.rs", "fn:src/x.rs:foo");
        assert!(
            !edges.iter().any(|e| e.id == contains),
            "lazy snapshot must NOT contain the file→function contains edge: {edges:?}"
        );
    }

    #[test]
    fn children_of_file_returns_direct_functions_not_grandchild_variables() {
        let mut graph = Graph::new();
        let _ = graph.apply_parsed(parse_rust_source("src/x.rs", "fn f() { let x = 1; }"));

        let (nodes, edges) = graph.children_of("file:src/x.rs");
        assert!(
            nodes.iter().any(|n| n.id == "fn:src/x.rs:f"),
            "expected the direct function child: {nodes:?}"
        );
        assert!(
            !nodes.iter().any(|n| n.id == "var:src/x.rs:f:x"),
            "grandchild variable must NOT appear in direct children: {nodes:?}"
        );
        let contains = edge_id("file:src/x.rs", "fn:src/x.rs:f");
        assert!(
            edges.iter().any(|e| e.id == contains),
            "expected the file→function contains edge: {edges:?}"
        );
    }

    #[test]
    fn children_of_function_returns_its_variable_children() {
        let mut graph = Graph::new();
        let _ = graph.apply_parsed(parse_rust_source("src/x.rs", "fn f() { let x = 1; }"));

        let (nodes, edges) = graph.children_of("fn:src/x.rs:f");
        assert!(
            nodes.iter().any(|n| n.id == "var:src/x.rs:f:x"),
            "expected the variable child of the function: {nodes:?}"
        );
        let contains = edge_id("fn:src/x.rs:f", "var:src/x.rs:f:x");
        assert!(
            edges.iter().any(|e| e.id == contains),
            "expected the function→variable contains edge: {edges:?}"
        );
    }

    #[test]
    fn subtree_wraps_direct_children_in_a_subtree_envelope() {
        let mut graph = Graph::new();
        let _ = graph.apply_parsed(parse_rust_source("src/x.rs", "fn f() { let x = 1; }"));

        let env = graph.subtree("file:src/x.rs");
        assert_eq!(env.event_type, EventType::Subtree);
        match env.payload {
            Payload::Subtree {
                parent_id, nodes, ..
            } => {
                assert_eq!(parent_id, "file:src/x.rs");
                assert!(
                    nodes.iter().any(|n| n.id == "fn:src/x.rs:f"),
                    "subtree nodes must include the direct function child: {nodes:?}"
                );
                assert!(
                    !nodes.iter().any(|n| n.id == "var:src/x.rs:f:x"),
                    "subtree must carry direct children only, not grandchildren: {nodes:?}"
                );
            }
            other => panic!("expected subtree payload, got {other:?}"),
        }
    }

    #[test]
    fn apply_parsed_emits_node_upsert_for_new_function() {
        let mut graph = Graph::new();
        let events = graph.apply_parsed(parse_rust_source("src/x.rs", "fn foo() {}"));

        let id = node_id(NodeType::Function, "src/x.rs", "foo");
        let node = upserted_node(&events, &id).expect("node.upsert for foo");
        assert_eq!(node.id, id);
    }

    #[test]
    fn apply_parsed_emits_node_remove_for_vanished_function() {
        let mut graph = Graph::new();
        let _ = graph.apply_parsed(parse_rust_source("src/x.rs", "fn foo() {}\nfn bar() {}"));
        let events = graph.apply_parsed(parse_rust_source("src/x.rs", "fn foo() {}"));

        let bar = node_id(NodeType::Function, "src/x.rs", "bar");
        assert!(
            removed_node_ids(&events).contains(&bar),
            "expected node.remove for {bar}, got {events:?}"
        );
    }

    #[test]
    fn reapplying_identical_parsed_file_emits_no_events() {
        let mut graph = Graph::new();
        let parsed = parse_rust_source("src/x.rs", "fn foo() {}\nfn bar() {}");
        let _ = graph.apply_parsed(parsed.clone());

        let events = graph.apply_parsed(parsed);
        assert!(events.is_empty(), "idempotent re-apply, got {events:?}");
    }

    /// Seeds a graph from one Rust function so `fn:a.rs:f` exists with status
    /// [`NodeStatus::Unknown`], mirroring how the other tests build a graph.
    fn graph_with_function() -> Graph {
        let mut graph = Graph::new();
        let _ = graph.apply_parsed(parse_rust_source("a.rs", "fn f() {}"));
        graph
    }

    fn test_event(node: &str, outcome: TestOutcome) -> ClvEvent {
        ClvEvent::Test {
            session: "s1".to_string(),
            pid: None,
            agent: None,
            msg: None,
            node: node.to_string(),
            outcome,
            duration_ms: None,
        }
    }

    fn status_event(node: &str, outcome: TestOutcome) -> ClvEvent {
        ClvEvent::Status {
            session: "s1".to_string(),
            pid: None,
            agent: None,
            msg: None,
            node: node.to_string(),
            outcome,
        }
    }

    /// Reads the stored status of a direct child of `file:a.rs` via the public
    /// lazy-children path, proving a later `subtree` reflects the colour.
    fn child_status(graph: &Graph, id: &str) -> Option<NodeStatus> {
        graph
            .children_of("file:a.rs")
            .0
            .into_iter()
            .find(|n| n.id == id)
            .map(|n| n.status)
    }

    #[test]
    fn apply_clv_test_fail_colours_node_failing_and_returns_test_result() {
        let mut graph = graph_with_function();
        let env = graph
            .apply_clv(&test_event("fn:a.rs:f", TestOutcome::Fail))
            .into_iter()
            .next()
            .expect("test event for an existing node returns an envelope");

        assert_eq!(env.event_type, EventType::TestResult);
        match &env.payload {
            Payload::TestResult { node_id, .. } => assert_eq!(node_id, "fn:a.rs:f"),
            other => panic!("expected TestResult payload, got {other:?}"),
        }
        assert_eq!(child_status(&graph, "fn:a.rs:f"), Some(NodeStatus::Failing));
    }

    #[test]
    fn apply_clv_test_pass_flips_stored_status_to_passing() {
        let mut graph = graph_with_function();
        let _ = graph.apply_clv(&test_event("fn:a.rs:f", TestOutcome::Fail));

        let env = graph
            .apply_clv(&test_event("fn:a.rs:f", TestOutcome::Pass))
            .into_iter()
            .next()
            .expect("test event for an existing node returns an envelope");
        assert_eq!(env.event_type, EventType::TestResult);
        assert_eq!(child_status(&graph, "fn:a.rs:f"), Some(NodeStatus::Passing));
    }

    #[test]
    fn apply_clv_test_skip_colours_node_stale() {
        let mut graph = graph_with_function();
        let env = graph
            .apply_clv(&test_event("fn:a.rs:f", TestOutcome::Skip))
            .into_iter()
            .next()
            .expect("test event for an existing node returns an envelope");
        assert_eq!(env.event_type, EventType::TestResult);
        assert_eq!(child_status(&graph, "fn:a.rs:f"), Some(NodeStatus::Stale));
    }

    #[test]
    fn apply_clv_status_running_sets_running_and_returns_status_update() {
        let mut graph = graph_with_function();
        let env = graph
            .apply_clv(&status_event("fn:a.rs:f", TestOutcome::Running))
            .into_iter()
            .next()
            .expect("status event for an existing node returns an envelope");

        assert_eq!(env.event_type, EventType::StatusUpdate);
        match &env.payload {
            Payload::StatusUpdate {
                node_id, status, ..
            } => {
                assert_eq!(node_id, "fn:a.rs:f");
                assert_eq!(*status, NodeStatus::Running);
            }
            other => panic!("expected StatusUpdate payload, got {other:?}"),
        }
        assert_eq!(child_status(&graph, "fn:a.rs:f"), Some(NodeStatus::Running));
    }

    #[test]
    fn apply_clv_absent_node_returns_none_and_leaves_graph_unchanged() {
        let mut graph = graph_with_function();
        let before = graph.nodes.clone();

        let result = graph.apply_clv(&test_event("fn:does.not.exist", TestOutcome::Fail));
        assert!(result.is_empty(), "absent node must yield no envelope");
        assert_eq!(
            graph.nodes, before,
            "graph must be untouched for an absent node"
        );

        // The same contract holds for a status event.
        let result = graph.apply_clv(&status_event("fn:does.not.exist", TestOutcome::Running));
        assert!(
            result.is_empty(),
            "absent node must yield no envelope for status"
        );
        assert_eq!(
            graph.nodes, before,
            "graph must be untouched for an absent node"
        );
    }

    #[test]
    fn apply_clv_activity_is_a_noop_for_colour() {
        let mut graph = graph_with_function();
        let before = graph.nodes.clone();

        let result = graph.apply_clv(&ClvEvent::Activity {
            session: "s1".to_string(),
            pid: None,
            agent: None,
            msg: None,
            node: "fn:a.rs:f".to_string(),
            action: "modified".to_string(),
        });
        assert!(
            result.is_empty(),
            "an unattributable activity (no agent/pid) must yield no envelope"
        );
        assert_eq!(graph.nodes, before, "activity must not change node colour");
        assert_eq!(child_status(&graph, "fn:a.rs:f"), Some(NodeStatus::Unknown));
    }

    /// Builds a cold `calls` edge (`hot: false`) with the given id so the hot-edge
    /// tests have a target already in the graph.
    fn cold_edge(id: &str) -> Edge {
        Edge {
            id: id.to_string(),
            source: "fn:a.rs:f".to_string(),
            target: "fn:a.rs:g".to_string(),
            kind: EdgeKind::Calls,
            hot: false,
        }
    }

    /// Builds a `hotedge` CLV event echoing a fixed session/pid/agent so payload
    /// pass-through can be asserted.
    fn hotedge_event(edge: &str, state: &str) -> ClvEvent {
        ClvEvent::HotEdge {
            session: "s1".to_string(),
            pid: Some(42),
            agent: Some("agent-x".to_string()),
            msg: None,
            edge: edge.to_string(),
            state: state.to_string(),
        }
    }

    /// Reads the stored `hot` flag of an edge by id via the graph's private map.
    fn edge_hot(graph: &Graph, id: &str) -> Option<bool> {
        graph.edges.get(id).map(|e| e.hot)
    }

    #[test]
    fn apply_clv_hotedge_enter_toggles_edge_hot_and_emits_envelope() {
        let mut graph = graph_with_function();
        graph.upsert_edge(cold_edge("e:test"));

        let env = graph
            .apply_clv(&hotedge_event("e:test", "enter"))
            .into_iter()
            .next()
            .expect("enter on an existing cold edge returns an envelope");

        assert_eq!(env.event_type, EventType::HotEdge);
        match &env.payload {
            Payload::HotEdge {
                edge_id,
                state,
                session_id,
                process_id,
                agent_id,
                ..
            } => {
                assert_eq!(edge_id, "e:test");
                assert_eq!(*state, HotEdgeState::Enter);
                assert_eq!(session_id, "s1");
                assert_eq!(*process_id, Some(42));
                assert_eq!(agent_id.as_deref(), Some("agent-x"));
            }
            other => panic!("expected HotEdge payload, got {other:?}"),
        }
        assert_eq!(edge_hot(&graph, "e:test"), Some(true), "edge is now hot");
    }

    #[test]
    fn apply_clv_hotedge_re_enter_on_hot_edge_coalesces_to_none() {
        let mut graph = graph_with_function();
        graph.upsert_edge(cold_edge("e:test"));
        let _ = graph
            .apply_clv(&hotedge_event("e:test", "enter"))
            .into_iter()
            .next()
            .expect("first enter returns an envelope");

        // Transition-coalescing: a second enter on an already-hot edge emits nothing.
        let result = graph.apply_clv(&hotedge_event("e:test", "enter"));
        assert!(
            result.is_empty(),
            "re-entering a hot edge must yield no envelope"
        );
        assert_eq!(edge_hot(&graph, "e:test"), Some(true), "edge stays hot");
    }

    #[test]
    fn apply_clv_hotedge_exit_clears_hot_then_re_exit_coalesces() {
        let mut graph = graph_with_function();
        graph.upsert_edge(cold_edge("e:test"));
        let _ = graph
            .apply_clv(&hotedge_event("e:test", "enter"))
            .into_iter()
            .next()
            .expect("enter returns an envelope");

        let env = graph
            .apply_clv(&hotedge_event("e:test", "exit"))
            .into_iter()
            .next()
            .expect("exit on a hot edge returns an envelope");
        assert_eq!(env.event_type, EventType::HotEdge);
        match &env.payload {
            Payload::HotEdge { state, .. } => assert_eq!(*state, HotEdgeState::Exit),
            other => panic!("expected HotEdge payload, got {other:?}"),
        }
        assert_eq!(edge_hot(&graph, "e:test"), Some(false), "edge is now cold");

        // A second exit on the already-cold edge coalesces to None.
        let result = graph.apply_clv(&hotedge_event("e:test", "exit"));
        assert!(
            result.is_empty(),
            "re-exiting a cold edge must yield no envelope"
        );
        assert_eq!(edge_hot(&graph, "e:test"), Some(false), "edge stays cold");
    }

    #[test]
    fn apply_clv_hotedge_unknown_edge_returns_none_and_mutates_nothing() {
        let mut graph = graph_with_function();
        graph.upsert_edge(cold_edge("e:test"));
        let before = graph.edges.clone();

        let result = graph.apply_clv(&hotedge_event("e:absent", "enter"));
        assert!(result.is_empty(), "unknown edge id must yield no envelope");
        assert_eq!(
            graph.edges, before,
            "no edge may be mutated for an absent id"
        );
    }

    #[test]
    fn apply_clv_hotedge_unknown_state_returns_none_and_mutates_nothing() {
        let mut graph = graph_with_function();
        graph.upsert_edge(cold_edge("e:test"));
        let before = graph.edges.clone();

        // Any state word other than enter/exit is ignored panic-free.
        let result = graph.apply_clv(&hotedge_event("e:test", "wat"));
        assert!(result.is_empty(), "garbage state must yield no envelope");
        assert_eq!(graph.edges, before, "garbage state must not toggle hot");
    }

    #[test]
    fn apply_clv_hotedge_never_changes_node_status() {
        let mut graph = graph_with_function();
        graph.upsert_edge(cold_edge("e:test"));
        let before = graph.nodes.clone();

        let _ = graph.apply_clv(&hotedge_event("e:test", "enter"));
        let _ = graph.apply_clv(&hotedge_event("e:test", "exit"));
        let _ = graph.apply_clv(&hotedge_event("e:absent", "enter"));

        assert_eq!(
            graph.nodes, before,
            "hotedge touches only edges, never nodes"
        );
    }

    #[test]
    fn colouring_a_node_then_reparsing_same_source_keeps_structure() {
        let mut graph = graph_with_function();
        let _ = graph.apply_clv(&test_event("fn:a.rs:f", TestOutcome::Fail));

        // Re-parsing the identical source must keep the structural graph intact.
        let _ = graph.apply_parsed(parse_rust_source("a.rs", "fn f() {}"));

        let roots = snapshot_nodes(&graph.snapshot());
        assert!(
            roots.iter().any(|n| n.id == "file:a.rs"),
            "root file node must survive a re-parse: {roots:?}"
        );
        let (children, _) = graph.children_of("file:a.rs");
        assert!(
            children.iter().any(|n| n.id == "fn:a.rs:f"),
            "function child must survive a re-parse: {children:?}"
        );
    }

    // ---- P8-2: activity → agent node + authored_by edge + in-memory roster ----
    //
    // These tests pin the widened `apply_clv` contract (`-> Vec<EventEnvelope>`) and
    // the agent-layer side effects of an `activity` event. They are RED until P8-2
    // lands: every `let events: Vec<EventEnvelope> = graph.apply_clv(..)` is a
    // signature mismatch against today's `Option<EventEnvelope>` return, and the
    // roster/snapshot/escaping assertions have no implementation behind them yet.

    /// Builds a `modified` activity CLV event from `agent`/`pid` touching `node`.
    fn activity_event(node: &str, agent: &str, pid: u32) -> ClvEvent {
        ClvEvent::Activity {
            session: "s1".to_string(),
            pid: Some(pid),
            agent: Some(agent.to_string()),
            msg: Some("touched it".to_string()),
            node: node.to_string(),
            action: "modified".to_string(),
        }
    }

    /// Returns the first `authored_by` edge from an `edge.upsert` envelope, if any.
    fn authored_by_edge(events: &[EventEnvelope]) -> Option<Edge> {
        events
            .iter()
            .find_map(|env| match (&env.event_type, &env.payload) {
                (EventType::EdgeUpsert, Payload::EdgeUpsert { edge })
                    if edge.kind == EdgeKind::AuthoredBy =>
                {
                    Some(edge.clone())
                }
                _ => None,
            })
    }

    /// Extracts the `agent.roster` payload's agent rows, panicking if none was emitted.
    fn roster_agents(events: &[EventEnvelope]) -> Vec<AgentInfo> {
        events
            .iter()
            .find_map(|env| match (&env.event_type, &env.payload) {
                (EventType::AgentRoster, Payload::AgentRoster { agents, .. }) => {
                    Some(agents.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected an AgentRoster payload, got {events:?}"))
    }

    #[test]
    fn apply_clv_activity_emits_agent_node_authored_edge_activity_and_roster() {
        let mut graph = Graph::new();
        // Pins the widened signature: `apply_clv` must return `Vec<EventEnvelope>`.
        let events: Vec<EventEnvelope> =
            graph.apply_clv(&activity_event("fn:src/x.rs:foo", "tdd-green", 48213));

        // (a) a node.upsert for the agent vertex, typed NodeType::Agent.
        let agent_id = agent_node_id("tdd-green");
        let agent_node = upserted_node(&events, &agent_id)
            .unwrap_or_else(|| panic!("expected a node.upsert for {agent_id}, got {events:?}"));
        assert_eq!(
            agent_node.node_type,
            NodeType::Agent,
            "agent node is typed Agent"
        );

        // (b) an edge.upsert of kind authored_by from the code node to the agent node.
        let edge = authored_by_edge(&events)
            .unwrap_or_else(|| panic!("expected an authored_by edge.upsert, got {events:?}"));
        assert_eq!(edge.kind, EdgeKind::AuthoredBy);
        assert_eq!(
            edge.source, "fn:src/x.rs:foo",
            "edge sources at the code node"
        );
        assert_eq!(edge.target, agent_id, "edge targets the agent node");

        // (c) an agent.activity envelope and (d) an agent.roster envelope.
        assert!(
            events
                .iter()
                .any(|e| e.event_type == EventType::AgentActivity),
            "expected an AgentActivity envelope, got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e.event_type == EventType::AgentRoster),
            "expected an AgentRoster envelope, got {events:?}"
        );
    }

    #[test]
    fn apply_clv_activity_roster_lists_the_active_process() {
        let mut graph = Graph::new();
        let events: Vec<EventEnvelope> =
            graph.apply_clv(&activity_event("fn:src/x.rs:foo", "tdd-green", 48213));

        let agents = roster_agents(&events);
        let row = agents
            .iter()
            .find(|a| a.agent_id == "tdd-green" && a.process_id == 48213)
            .unwrap_or_else(|| panic!("expected a roster row for tdd-green/48213, got {agents:?}"));
        assert_eq!(row.status, "active", "the touched process is marked active");
    }

    #[test]
    fn apply_clv_second_activity_new_pid_adds_roster_row_reuses_node_and_edge_id() {
        let mut graph = Graph::new();
        let first: Vec<EventEnvelope> =
            graph.apply_clv(&activity_event("fn:src/x.rs:foo", "tdd-green", 48213));
        let second: Vec<EventEnvelope> =
            graph.apply_clv(&activity_event("fn:src/x.rs:foo", "tdd-green", 99999));

        // The second roster carries TWO process rows, both under the one agentId.
        let agents = roster_agents(&second);
        let rows: Vec<&AgentInfo> = agents
            .iter()
            .filter(|a| a.agent_id == "tdd-green")
            .collect();
        assert_eq!(rows.len(), 2, "two pids under one agentId, got {agents:?}");
        let mut pids: Vec<u32> = rows.iter().map(|a| a.process_id).collect();
        pids.sort_unstable();
        assert_eq!(
            pids,
            vec![48213, 99999],
            "both pids tracked under the agent"
        );

        // The agent vertex id is reused — exactly one agent node, no duplicate.
        let agent_id = agent_node_id("tdd-green");
        let agent_nodes: Vec<Node> = snapshot_nodes(&graph.snapshot())
            .into_iter()
            .filter(|n| n.id == agent_id)
            .collect();
        assert_eq!(
            agent_nodes.len(),
            1,
            "no duplicate agent node: {agent_nodes:?}"
        );

        // The authored_by edge id is deterministic/stable across the two activities.
        let e1 = authored_by_edge(&first).expect("first authored_by edge");
        let e2 = authored_by_edge(&second).expect("second authored_by edge");
        assert_eq!(
            e1.id, e2.id,
            "authored_by edge id must be deterministic/stable across activities"
        );
    }

    #[test]
    fn apply_clv_repeat_activity_same_pid_coalesces_roster_but_keeps_edge() {
        let mut graph = Graph::new();
        let _first: Vec<EventEnvelope> =
            graph.apply_clv(&activity_event("fn:src/x.rs:foo", "tdd-green", 48213));
        // A second identical touch (same pid, same agent identity, already "active")
        // is a no-change roster repeat: SPEC §11.2 coalescing skips the roster
        // rebuild + broadcast. The authored_by edge is still emitted on the re-touch.
        let repeat: Vec<EventEnvelope> =
            graph.apply_clv(&activity_event("fn:src/x.rs:foo", "tdd-green", 48213));

        assert!(
            !repeat
                .iter()
                .any(|e| e.event_type == EventType::AgentRoster),
            "a no-change roster repeat must not re-broadcast the roster, got {repeat:?}"
        );
        assert!(
            authored_by_edge(&repeat).is_some(),
            "the authored_by edge is still emitted on the repeat touch, got {repeat:?}"
        );
    }

    #[test]
    fn apply_clv_test_status_hotedge_each_return_a_single_element_vec() {
        // Test event → exactly one TestResult envelope (regression for the Vec widen).
        let mut graph = graph_with_function();
        let events: Vec<EventEnvelope> =
            graph.apply_clv(&test_event("fn:a.rs:f", TestOutcome::Fail));
        assert_eq!(
            events.len(),
            1,
            "test event yields one envelope, got {events:?}"
        );
        assert_eq!(events[0].event_type, EventType::TestResult);

        // Status event → exactly one StatusUpdate envelope.
        let mut graph = graph_with_function();
        let events: Vec<EventEnvelope> =
            graph.apply_clv(&status_event("fn:a.rs:f", TestOutcome::Running));
        assert_eq!(
            events.len(),
            1,
            "status event yields one envelope, got {events:?}"
        );
        assert_eq!(events[0].event_type, EventType::StatusUpdate);

        // HotEdge enter on a cold edge → exactly one HotEdge envelope.
        let mut graph = graph_with_function();
        graph.upsert_edge(cold_edge("e:test"));
        let events: Vec<EventEnvelope> = graph.apply_clv(&hotedge_event("e:test", "enter"));
        assert_eq!(
            events.len(),
            1,
            "hotedge yields one envelope, got {events:?}"
        );
        assert_eq!(events[0].event_type, EventType::HotEdge);
    }

    #[test]
    fn snapshot_includes_agent_root_node_but_not_the_authored_by_edge() {
        let mut graph = Graph::new();
        // Seed a genuine non-root function node (child of file:src/x.rs).
        let _ = graph.apply_parsed(parse_rust_source("src/x.rs", "fn foo() {}"));
        let _events: Vec<EventEnvelope> =
            graph.apply_clv(&activity_event("fn:src/x.rs:foo", "tdd-green", 48213));

        let snap = graph.snapshot();
        let nodes = snapshot_nodes(&snap);
        let agent_id = agent_node_id("tdd-green");
        let agent_node = nodes
            .iter()
            .find(|n| n.id == agent_id)
            .unwrap_or_else(|| panic!("agent node must appear as a snapshot root, got {nodes:?}"));
        assert!(
            agent_node.parent_id.is_none(),
            "agent node is a root (no parentId): {agent_node:?}"
        );
        assert_eq!(agent_node.node_type, NodeType::Agent);

        let edges = snapshot_edges(&snap);
        assert!(
            !edges.iter().any(|e| e.kind == EdgeKind::AuthoredBy),
            "snapshot must omit the authored_by edge (its source is a non-root function): {edges:?}"
        );
    }
}
