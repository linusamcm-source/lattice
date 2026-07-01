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
//! Because there is no process-exit signal, a process's `status` lifecycle is closed
//! by an **idle timeout**: each activity stamps the process's `last_seen` (via
//! [`Graph::apply_clv_at`]), and [`Graph::expire_idle`] flips any process quiet for
//! longer than [`ROSTER_IDLE_MS`] to `inactive`, re-broadcasting one `agent.roster`
//! only when a row changed.
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
//! Crash-rebuild warm start (Phase 9). [`Graph::from_records`] rehydrates a graph
//! from a [`Storage`](crate::storage::Storage) backend's persisted `nodes`/`edges`,
//! re-deriving the unpersisted `child_ids` from the loaded `contains` edges (canonically
//! sorted via the shared [`crate::wire::derive_child_ids`], so the result is independent
//! of the loaded edge order) and rebuilding the `file_nodes`/`file_edges` diff
//! bookkeeping — excluding the agent-layer overlays (`agent` vertices and `authored_by`
//! edges) exactly as the live path does — so a follow-up [`Graph::apply_parsed`] of an
//! unchanged file reconciles to a no-op with no spurious removals. The agent roster is
//! **not** restored (no table) — it repopulates from live activity.
//!
//! Self-observability (Phase 9). [`Graph::record_parse_latency`] stamps each file's
//! most-recent parse `duration_us` into a map bounded by the distinct-file count, and
//! [`Graph::metrics_payload`] renders a **clock-free, deterministic** `metrics.update`
//! [`Payload`] (live node/edge counts, a pure `memoryBytes` estimate, the passed-in
//! events/sec, and the per-file latencies); [`Graph::metrics_envelope`] is the public
//! wrapper that stamps the envelope's `v`/`ts`/`sessionId`. To keep the metrics emitter
//! off the graph mutex's critical section, both are built over [`Graph::metrics_snapshot`]
//! — a lock-light read of counts/memory/latencies — with the `parseLatency` sort and the
//! clock reading deferred to [`MetricsSnapshot::into_envelope`] after the guard is dropped.
//!
//! Per `AGENT_PROTOCOL.md` §6 this is panic-free: a clock error or a parse with no
//! `file` node degrades to a safe default rather than unwrapping.

use std::collections::{HashMap, HashSet};

use crate::clv::ClvEvent;
use crate::parser::ParsedFile;
use crate::wire::{
    agent_node_id, derive_child_ids, typed_edge_id, AgentInfo, Edge, EdgeKind, EventEnvelope,
    EventType, FileParseLatency, HotEdgeState, Node, NodeStatus, NodeType, Payload, TestOutcome,
};

/// CLV protocol version stamped on every envelope this graph emits.
const PROTOCOL_VERSION: u32 = 1;

/// Session id used when a [`Graph`] is created without an explicit one.
const DEFAULT_SESSION_ID: &str = "sess-local";

/// Idle window, in milliseconds, after which a quiet process is timed out.
///
/// The collector has no process-exit signal, so a roster row's `"inactive"`
/// state is an **idle timeout**: [`Graph::expire_idle`] flips any still-`"active"`
/// process whose most recent activity is *strictly older* than this window to
/// `"inactive"`. A process touched exactly `ROSTER_IDLE_MS` ago is still live (the
/// comparison is strict); one touched a millisecond longer ago is timed out. The
/// `now`/`last_seen` clock is the monotonic millisecond domain of
/// [`monotonic_now_ms`].
pub const ROSTER_IDLE_MS: u64 = 5_000;

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
    /// Last-seen monotonic timestamp (ms) per tracked **process id** (Phase 8).
    ///
    /// [`Graph::apply_clv_at`] records `now_ms` here on every `activity` carrying a
    /// pid, so [`Graph::expire_idle`] can time out a process that has gone quiet:
    /// a roster row whose `last_seen` is strictly older than [`ROSTER_IDLE_MS`] is
    /// flipped to `"inactive"`. Same monotonic-millisecond domain as
    /// [`monotonic_now_ms`].
    last_seen: HashMap<u32, u64>,
    /// Most-recent parse latency (microseconds) per repo-relative source path (Phase 9).
    ///
    /// [`Graph::record_parse_latency`] stamps a file's `duration_us` here on every
    /// parse, overwriting the prior value, so the map is **bounded by the number of
    /// distinct source files** — never by the number of edits. It is a pure
    /// self-observability read: [`Graph::metrics_payload`] snapshots it into the
    /// `metrics.update` envelope's `parseLatency` rows without a clock.
    parse_latency: HashMap<String, u64>,
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
            last_seen: HashMap::new(),
            parse_latency: HashMap::new(),
        }
    }

    /// Rebuilds a graph from persisted node/edge records (crash-rebuild warm start).
    ///
    /// Given the `nodes`/`edges` a [`Storage`](crate::storage::Storage) backend loaded
    /// for `session_id` ([`load_nodes`](crate::storage::Storage::load_nodes) /
    /// [`load_edges`](crate::storage::Storage::load_edges)), this bulk-loads the node
    /// and edge maps and reconstructs the diff-tracking bookkeeping so a subsequent
    /// [`Graph::apply_parsed`] of an **unchanged** file is a no-op (the reconcile path,
    /// Design Decision #1).
    ///
    /// **Re-derives each node's `child_ids` from the loaded `contains` edges (Design
    /// Decision #7).** `child_ids` is part of a [`Node`]'s identity (and the
    /// [`Graph::apply_parsed`] byte-equality no-op check) but has **no persisted
    /// column**, so [`load_nodes`](crate::storage::Storage::load_nodes) returns it
    /// empty. This applies the shared [`derive_child_ids`] rule — the **same** one the
    /// parser's `populate_child_ids` uses — which sorts each node's `child_ids`
    /// **canonically by child id**. The sort is what makes this **order-independent**:
    /// neither backend's `load_edges` guarantees an order, so deriving in raw edge order
    /// would yield a different permutation than the parser produced and spuriously fail
    /// the no-op check; sorting makes a rebuilt node byte-equal to a freshly parsed one
    /// regardless of loaded edge order, so re-parsing the same source emits no spurious
    /// upsert.
    ///
    /// **Rebuilds `file_nodes`/`file_edges`** (the per-file id sets
    /// [`Graph::apply_parsed`] uses to compute removals): a node is grouped under the
    /// root `file:` node reached by walking its `parent_id` chain, and an edge under
    /// the file of its **source** node (so cross-file `calls`/`data_flows_from`/
    /// `authored_by` edges are attributed to the file that produces them, not removed
    /// when another file re-parses).
    ///
    /// The `roster`/`last_seen` maps start **empty**: the agent roster has no table and
    /// is **not restored** here — it is repopulated by live activity after restart
    /// (Design Decision #1). Never panics.
    pub fn from_records(session_id: impl Into<String>, nodes: Vec<Node>, edges: Vec<Edge>) -> Self {
        let mut graph = Self::with_session(session_id);

        // Re-derive child_ids canonically (sorted, backend-independent) from the
        // `contains` edges (DD#7) — the same shared rule the parser applies. Move each
        // child list out of the map (rather than clone) as it is consumed.
        let mut children = derive_child_ids(&edges);
        for mut node in nodes {
            node.child_ids = children.remove(&node.id).unwrap_or_default();
            graph.nodes.insert(node.id.clone(), node);
        }
        for edge in edges {
            graph.edges.insert(edge.id.clone(), edge);
        }

        graph.rebuild_file_tracking();
        graph
    }

    /// Rebuilds the `file_nodes`/`file_edges` diff-tracking maps from the current
    /// node/edge maps, grouping each node under its owning `file:` root and each edge
    /// under its **source** node's file (Design Decision #7). Used by
    /// [`Graph::from_records`] after a bulk load.
    ///
    /// **Agent-layer overlays are excluded**, mirroring exactly what [`Graph::apply_parsed`]
    /// file-tracks: [`NodeType::Agent`] vertices and [`EdgeKind::AuthoredBy`] edges are the
    /// live-overlay product of [`Graph::apply_clv`] and are **never** recorded in
    /// `file_nodes`/`file_edges`. Tracking them here would make a follow-up reparse of an
    /// agent-touched file treat the agent vertex / `authored_by` edge as a vanished
    /// file element and emit a spurious `node.remove`/`edge.remove`.
    fn rebuild_file_tracking(&mut self) {
        let mut file_nodes: HashMap<String, Vec<String>> = HashMap::new();
        for node in self.nodes.values() {
            // Agent vertices are live-overlay, not file-tracked by `apply_parsed`.
            if node.node_type == NodeType::Agent {
                continue;
            }
            if let Some(file_id) = self.owning_file(&node.id) {
                file_nodes.entry(file_id).or_default().push(node.id.clone());
            }
        }
        let mut file_edges: HashMap<String, Vec<String>> = HashMap::new();
        for edge in self.edges.values() {
            // `authored_by` edges are live-overlay, not file-tracked by `apply_parsed`.
            if edge.kind == EdgeKind::AuthoredBy {
                continue;
            }
            if let Some(file_id) = self.owning_file(&edge.source) {
                file_edges.entry(file_id).or_default().push(edge.id.clone());
            }
        }
        self.file_nodes = file_nodes;
        self.file_edges = file_edges;
    }

    /// Returns the id of the root `file:` node that owns `node_id`, by walking the
    /// `parent_id` chain to its topmost present ancestor (a node with no `parent_id`,
    /// i.e. the file). Returns `None` when `node_id` is unknown. A bounded loop guards
    /// against a malformed parent cycle, so it never hangs or panics.
    fn owning_file(&self, node_id: &str) -> Option<String> {
        let mut current = self.nodes.get(node_id)?;
        for _ in 0..=self.nodes.len() {
            match current.parent_id.as_deref().and_then(|p| self.nodes.get(p)) {
                Some(parent) => current = parent,
                None => return Some(current.id.clone()),
            }
        }
        Some(current.id.clone())
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
    /// The live-overlay entry point: delegates to [`Graph::apply_clv_at`] with a
    /// fresh [`monotonic_now_ms`] reading, so an `activity` touch stamps its
    /// process's `last_seen` on the same monotonic clock [`Graph::expire_idle`]
    /// later reads. See [`Graph::apply_clv_at`] for the full event-folding contract;
    /// this thin wrapper exists only so the production tail takes the real clock
    /// while tests inject `now_ms`. Panic-free.
    pub fn apply_clv(&mut self, event: &ClvEvent) -> Vec<EventEnvelope> {
        self.apply_clv_at(event, monotonic_now_ms())
    }

    /// Applies one correlated CLV event with the supplied monotonic clock reading.
    ///
    /// The timestamped seam behind [`Graph::apply_clv`]: identical event-folding
    /// behaviour, but `now_ms` (a [`monotonic_now_ms`]-domain millisecond reading) is
    /// injectable so the roster idle timer is deterministically testable. Before the
    /// event is folded, an [`ClvEvent::Activity`] carrying a `pid` records
    /// `last_seen[pid] = now_ms`, the liveness stamp [`Graph::expire_idle`] later
    /// compares against [`ROSTER_IDLE_MS`].
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
    pub fn apply_clv_at(&mut self, event: &ClvEvent, now_ms: u64) -> Vec<EventEnvelope> {
        // Stamp process liveness before folding, so a quiet process can later be
        // timed out by `expire_idle`. Only an *attributed* (agent + pid) activity is
        // tracked — the same condition under which `apply_activity` rosters the
        // process — so an agent-less or pid-less line never leaves an orphan
        // last-seen entry that no roster row reads. Other events do not refresh the
        // last-seen clock.
        if let ClvEvent::Activity {
            pid: Some(pid),
            agent: Some(_),
            ..
        } = event
        {
            self.last_seen.insert(*pid, now_ms);
        }
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
            events.push(self.roster_envelope(session.to_string()));
        }

        events
    }

    /// Times out every quiet process and returns the rebuilt roster on any change.
    ///
    /// The collector has no process-exit signal, so `"inactive"` is an **idle
    /// timeout**: this flips every still-`"active"` roster row whose `last_seen`
    /// (recorded by [`Graph::apply_clv_at`]) is **strictly older** than
    /// [`ROSTER_IDLE_MS`] relative to `now_ms` to `"inactive"`. A process touched
    /// exactly one window ago is *not* expired (the comparison is strict). When at
    /// least one row changed it returns **exactly one** full `agent.roster` snapshot
    /// envelope (every row, sorted by pid); when nothing changed — including a repeat
    /// call after a process is already `"inactive"` — it returns an empty vector and
    /// broadcasts nothing. `now_ms` shares the monotonic-millisecond domain of
    /// [`monotonic_now_ms`]; a `last_seen` ahead of `now_ms` is treated as age zero
    /// (saturating), so it is panic-free.
    pub fn expire_idle(&mut self, now_ms: u64) -> Vec<EventEnvelope> {
        let mut changed = false;
        for (pid, info) in self.roster.iter_mut() {
            if info.status != "active" {
                continue;
            }
            let last_seen = self.last_seen.get(pid).copied().unwrap_or(0);
            if now_ms.saturating_sub(last_seen) > ROSTER_IDLE_MS {
                info.status = "inactive".to_string();
                changed = true;
            }
        }
        if changed {
            vec![self.roster_envelope(self.session_id.clone())]
        } else {
            Vec::new()
        }
    }

    /// Builds one full `agent.roster` snapshot envelope stamped with `session_id`.
    ///
    /// Clones every [`AgentInfo`] roster row and sorts it by `process_id` for a
    /// stable order, then wraps it in an [`EventType::AgentRoster`] envelope. Shared
    /// by [`Graph::apply_activity`] (a new/changed process), [`Graph::expire_idle`]
    /// (a timed-out process) and [`Graph::roster_snapshot`] (the connect/resync
    /// trailer) so every path broadcasts an identical full-roster shape.
    fn roster_envelope(&self, session_id: String) -> EventEnvelope {
        let mut agents: Vec<AgentInfo> = self.roster.values().cloned().collect();
        agents.sort_by_key(|a| a.process_id);
        self.envelope(
            EventType::AgentRoster,
            Payload::AgentRoster { session_id, agents },
        )
    }

    /// Returns the current roster as an `agent.roster` envelope, or `None` when empty.
    ///
    /// The connect/resync trailer accessor (Design Decision #4, P9-7): a `snapshot`
    /// is root-only and never carries agent-layer state, so a client connecting or
    /// resyncing mid-run would see agent nodes but an empty roster. This exposes the
    /// live [`Graph::roster`] as one full [`EventType::AgentRoster`] envelope (built
    /// via [`Graph::roster_envelope`], stamped with the graph's `session_id`) for
    /// [`handle_connection`](crate::ws) to send **after** the snapshot. Returns
    /// `None` when the roster is empty, so an empty-roster connect emits no spurious
    /// trailing `agent.roster`. Read-only and panic-free.
    pub fn roster_snapshot(&self) -> Option<EventEnvelope> {
        if self.roster.is_empty() {
            return None;
        }
        Some(self.roster_envelope(self.session_id.clone()))
    }

    /// Records `micros` as the most-recent parse latency for repo-relative `path`.
    ///
    /// Overwrites any prior value for the same path, so re-parsing a file does **not**
    /// grow the map — it stays bounded by the number of distinct source files, never by
    /// the number of edits (Phase 9 self-observability, `SPEC.md` §11.3). The recorded
    /// durations surface via [`Graph::metrics_payload`]'s `parseLatency` rows.
    pub fn record_parse_latency(&mut self, path: &str, micros: u64) {
        self.parse_latency.insert(path.to_string(), micros);
    }

    /// Builds a deterministic `metrics.update` [`Payload`] over the current graph state.
    ///
    /// **Clock-free and pure** (Design Decision #3): given the same nodes, edges and
    /// recorded parse latencies it always returns the same scalars, so it is
    /// unit-testable without timers. `nodeCount`/`edgeCount` mirror the live map sizes,
    /// `memoryBytes` is the deterministic [`Graph::estimated_memory_bytes`] estimate
    /// (not platform RSS), `eventsPerSecMilli` is `events_per_sec_milli` verbatim, and
    /// `parseLatency` lists each recorded file (sorted by path for a stable order) with
    /// its most-recent `durationUs`. The payload's `ts` is left **empty** here — the
    /// authoritative timestamp is stamped by [`Graph::metrics_envelope`] — so this
    /// method takes no clock reading. Implemented over [`Graph::metrics_snapshot`], so
    /// the `parseLatency` sort lives in [`MetricsSnapshot::into_payload`] rather than
    /// under any lock a caller holds while reading `&self`.
    pub fn metrics_payload(&self, events_per_sec_milli: u64) -> Payload {
        self.metrics_snapshot()
            .into_payload(events_per_sec_milli, String::new())
    }

    /// Wraps [`Graph::metrics_payload`] in a fully stamped `metrics.update` envelope.
    ///
    /// Reads the clock **once**, stamps that timestamp into both the envelope and the
    /// payload's `ts`, and returns the same [`EventEnvelope`] as before. The metrics
    /// emitter no longer calls this **under** the graph lock — it takes a lock-light
    /// [`Graph::metrics_snapshot`], drops the guard, then assembles the envelope off-lock
    /// via [`MetricsSnapshot::into_envelope`]. This method is retained (delegating through
    /// that same snapshot) so its signature and result are unchanged for any other caller.
    pub fn metrics_envelope(&self, events_per_sec_milli: u64) -> EventEnvelope {
        self.metrics_snapshot().into_envelope(events_per_sec_milli)
    }

    /// Captures the minimal graph state a `metrics.update` needs, reading **no clock and
    /// doing no sort** while `&self` is borrowed.
    ///
    /// The lock-contention seam for the Phase-9 metrics emitter (`SPEC.md` §11.3): the
    /// emitter holds the graph [`Mutex`](tokio::sync::Mutex) only long enough to read the
    /// live node/edge counts, the deterministic [`Graph::estimated_memory_bytes`]
    /// estimate, the session id, and a snapshot of the per-file parse-latency entries —
    /// then drops the guard and does the sort + clock stamp + envelope assembly off-lock
    /// via [`MetricsSnapshot::into_envelope`]. Keeping the sort and clock reading out of
    /// the critical section stops the emitter contending with the watcher and collector
    /// on the same mutex. The captured `parse_latency` is left **unsorted** here.
    pub(crate) fn metrics_snapshot(&self) -> MetricsSnapshot {
        let parse_latency: Vec<FileParseLatency> = self
            .parse_latency
            .iter()
            .map(|(file_path, &duration_us)| FileParseLatency {
                file_path: file_path.clone(),
                duration_us,
            })
            .collect();
        MetricsSnapshot {
            session_id: self.session_id.clone(),
            node_count: self.nodes.len() as u64,
            edge_count: self.edges.len() as u64,
            memory_bytes: self.estimated_memory_bytes(),
            parse_latency,
        }
    }

    /// Deterministic estimate of this graph's structural memory use, in bytes.
    ///
    /// A **pure function of the map sizes** (Design Decision #3): each of the
    /// `nodes`/`edges`/`parse_latency` counts times that element type's fixed stack
    /// size. Same graph state → same value on every call, so [`Graph::metrics_payload`]
    /// is unit-testable. This is a self-observability estimate, **not** platform RSS: it
    /// deliberately undercounts heap-owned strings in exchange for determinism.
    fn estimated_memory_bytes(&self) -> u64 {
        let node_bytes = self.nodes.len() as u64 * std::mem::size_of::<Node>() as u64;
        let edge_bytes = self.edges.len() as u64 * std::mem::size_of::<Edge>() as u64;
        let latency_bytes =
            self.parse_latency.len() as u64 * std::mem::size_of::<FileParseLatency>() as u64;
        node_bytes + edge_bytes + latency_bytes
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

/// A minimal owned snapshot of the graph state a `metrics.update` needs.
///
/// Produced by [`Graph::metrics_snapshot`] while the graph lock is held, so the sort,
/// clock reading, and envelope assembly can all run **after** the guard is dropped — the
/// Phase-9 fix that keeps the metrics emitter from contending with the watcher and
/// collector on the graph mutex (`SPEC.md` §11.3). `parse_latency` is captured
/// **unsorted** here and ordered off-lock by [`MetricsSnapshot::into_payload`].
pub(crate) struct MetricsSnapshot {
    session_id: String,
    node_count: u64,
    edge_count: u64,
    memory_bytes: u64,
    parse_latency: Vec<FileParseLatency>,
}

impl MetricsSnapshot {
    /// Builds the deterministic `metrics.update` [`Payload`] from this snapshot, stamping
    /// the payload's `ts` with `ts` (empty for the clock-free [`Graph::metrics_payload`]
    /// path). Sorts the captured `parse_latency` by `file_path` for a stable order — this
    /// is the sort lifted out of the graph lock's critical section, so the same graph
    /// state always yields the same ordered `parseLatency` regardless of map iteration.
    fn into_payload(mut self, events_per_sec_milli: u64, ts: String) -> Payload {
        self.parse_latency
            .sort_by(|a, b| a.file_path.cmp(&b.file_path));
        Payload::MetricsUpdate {
            session_id: self.session_id,
            ts,
            node_count: self.node_count,
            edge_count: self.edge_count,
            memory_bytes: self.memory_bytes,
            events_per_sec_milli,
            parse_latency: self.parse_latency,
        }
    }

    /// Assembles a fully stamped `metrics.update` [`EventEnvelope`] from this snapshot,
    /// reading the clock **once** off-lock and stamping that timestamp into both the
    /// envelope and the payload's `ts`. This is the emitter's off-lock path; it returns
    /// the same envelope [`Graph::metrics_envelope`] would for the same graph state.
    pub(crate) fn into_envelope(self, events_per_sec_milli: u64) -> EventEnvelope {
        let ts = rfc3339_now();
        let session_id = self.session_id.clone();
        let payload = self.into_payload(events_per_sec_milli, ts.clone());
        EventEnvelope {
            v: PROTOCOL_VERSION,
            ts,
            session_id,
            event_type: EventType::MetricsUpdate,
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

/// Returns milliseconds elapsed on a process-monotonic clock.
///
/// The real-clock source for the roster idle timer: [`Graph::apply_clv`] stamps a
/// process's `last_seen` with this, and the collector's tick passes it to
/// [`Graph::expire_idle`], so both sit in one monotonic domain immune to wall-clock
/// jumps. The zero point is a process-start [`std::time::Instant`] anchor captured
/// once on first call; tests bypass this and inject `now_ms` directly via
/// [`Graph::apply_clv_at`]/[`Graph::expire_idle`]. Panic-free.
pub(crate) fn monotonic_now_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    // Process-start anchor: the monotonic zero point shared by every reading.
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::Graph;
    use crate::clv::ClvEvent;
    use crate::parser::{parse_rust_source, parse_source};
    use crate::wire::{
        agent_node_id, edge_id, node_id, typed_edge_id, AgentInfo, Edge, EdgeKind, EventEnvelope,
        EventType, FileParseLatency, HotEdgeState, Node, NodeStatus, NodeType, Payload,
        TestOutcome,
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

    // ---- P9-7: Graph::roster_snapshot() accessor for connect/resync trailer ----
    //
    // These pin the new `pub fn Graph::roster_snapshot(&self) -> Option<EventEnvelope>`
    // accessor the ws.rs connect/resync trailer is built from (Design Decision #4).
    // RED until P9-7 lands: `roster_snapshot` does not yet exist, so the two calls
    // below are E0599 method-not-found errors and the whole test binary fails to
    // compile.

    /// An empty roster must yield `None` — no spurious empty `agent.roster`.
    #[test]
    fn roster_snapshot_is_none_when_the_roster_is_empty() {
        let graph = Graph::new();
        assert!(
            graph.roster_snapshot().is_none(),
            "an empty roster must yield no agent.roster envelope"
        );
    }

    /// After seeding the roster via the Phase-8 activity path, `roster_snapshot`
    /// returns `Some(agent.roster)` listing the seeded process.
    #[test]
    fn roster_snapshot_carries_the_seeded_roster() {
        let mut graph = Graph::new();
        // Seed the roster via the Phase-8 activity path (`apply_clv`).
        let _ = graph.apply_clv(&activity_event("fn:src/x.rs:foo", "tdd-green", 48213));

        let env = graph
            .roster_snapshot()
            .expect("a non-empty roster yields an agent.roster envelope");
        assert_eq!(
            env.event_type,
            EventType::AgentRoster,
            "roster_snapshot wraps an agent.roster envelope, got {env:?}"
        );
        let agents = roster_agents(std::slice::from_ref(&env));
        assert!(
            agents
                .iter()
                .any(|a| a.agent_id == "tdd-green" && a.process_id == 48213),
            "roster_snapshot lists the seeded process, got {agents:?}"
        );
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

    // ---- P8-4: roster idle-timeout → `inactive`, plus respawn ----------------
    //
    // The collector has no process-exit signal, so `inactive` is an idle-timeout.
    // P8-4 records a per-pid `last_seen` on each activity and adds
    // `Graph::expire_idle(now_ms)`, which flips any still-`active` process whose
    // last activity is **strictly older** than the named window `ROSTER_IDLE_MS`
    // to `status: "inactive"` and returns the full `agent.roster` snapshot —
    // exactly one envelope, and **only** when at least one row changed.
    //
    // Time is injectable so these tests are deterministic with NO real sleeps:
    //   * `Graph::apply_clv_at(&event, now_ms) -> Vec<EventEnvelope>` records the
    //     activity's `last_seen = now_ms` (production `apply_clv` delegates with a
    //     real monotonic now), and
    //   * `Graph::expire_idle(now_ms) -> Vec<EventEnvelope>` compares against an
    //     injected `now_ms` (same monotonic-millisecond domain).
    //
    // RED until P8-4 lands: `ROSTER_IDLE_MS`, `Graph::apply_clv_at`, and
    // `Graph::expire_idle` do not yet exist, so this module fails to compile.

    /// Finds the roster row for `pid` in `agents`, panicking if it is absent.
    fn roster_row(agents: &[AgentInfo], pid: u32) -> &AgentInfo {
        agents
            .iter()
            .find(|a| a.process_id == pid)
            .unwrap_or_else(|| panic!("expected a roster row for pid {pid}, got {agents:?}"))
    }

    #[test]
    fn expire_idle_flips_a_stale_process_to_inactive_then_coalesces() {
        let mut graph = Graph::new();
        // Record an activity at t0, establishing last_seen[48213] = t0.
        let t0: u64 = 1_000;
        let _ = graph.apply_clv_at(&activity_event("fn:src/x.rs:foo", "tdd-green", 48213), t0);

        // At t0 + window + 1 the process is strictly older than the idle window,
        // so expire_idle flips it to inactive and emits exactly one agent.roster.
        let now = t0 + super::ROSTER_IDLE_MS + 1;
        let expired: Vec<EventEnvelope> = graph.expire_idle(now);
        let rosters = expired
            .iter()
            .filter(|e| e.event_type == EventType::AgentRoster)
            .count();
        assert_eq!(
            rosters, 1,
            "exactly one agent.roster on the first expiry, got {expired:?}"
        );
        let agents = roster_agents(&expired);
        assert_eq!(
            roster_row(&agents, 48213).status,
            "inactive",
            "the stale process is marked inactive, got {agents:?}"
        );

        // Calling again with no further change yields an EMPTY vec — no roster
        // envelope is emitted when nothing changed (already inactive).
        let again: Vec<EventEnvelope> = graph.expire_idle(now);
        assert!(
            again.is_empty(),
            "a second expiry with no change emits nothing, got {again:?}"
        );
    }

    #[test]
    fn expire_idle_marks_only_the_idle_process_inactive() {
        let mut graph = Graph::new();
        // pid 100 last touched at t_old; pid 200 last touched a full window later.
        let t_old: u64 = 1_000;
        let t_fresh: u64 = t_old + super::ROSTER_IDLE_MS;
        let _ = graph.apply_clv_at(&activity_event("fn:src/x.rs:foo", "agent-old", 100), t_old);
        let _ = graph.apply_clv_at(
            &activity_event("fn:src/x.rs:foo", "agent-new", 200),
            t_fresh,
        );

        // `now` is one tick past the window for pid 100 (age = window + 1) but
        // only 1 ms past pid 200's touch (age = 1, well within the window).
        let now = t_fresh + 1;
        let expired: Vec<EventEnvelope> = graph.expire_idle(now);
        let agents = roster_agents(&expired);
        assert_eq!(
            roster_row(&agents, 100).status,
            "inactive",
            "the idle process flips to inactive, got {agents:?}"
        );
        assert_eq!(
            roster_row(&agents, 200).status,
            "active",
            "the fresh process stays active, got {agents:?}"
        );
    }

    #[test]
    fn expire_idle_then_new_pid_same_agent_keeps_old_inactive_and_new_active() {
        let mut graph = Graph::new();
        // The original process touches a node, then goes idle and is expired.
        let t0: u64 = 1_000;
        let _ = graph.apply_clv_at(&activity_event("fn:src/x.rs:foo", "tdd-green", 111), t0);
        let now = t0 + super::ROSTER_IDLE_MS + 1;
        let _ = graph.expire_idle(now); // pid 111 → inactive

        // Respawn: a NEW pid under the SAME agentId emits a fresh activity at `now`.
        // Its roster carries BOTH pids under the one agentId — the old one still
        // inactive, the new one active.
        let respawn: Vec<EventEnvelope> =
            graph.apply_clv_at(&activity_event("fn:src/x.rs:foo", "tdd-green", 222), now);
        let agents = roster_agents(&respawn);
        let rows = agents.iter().filter(|a| a.agent_id == "tdd-green").count();
        assert_eq!(
            rows, 2,
            "both pids tracked under one agentId, got {agents:?}"
        );
        assert_eq!(
            roster_row(&agents, 111).status,
            "inactive",
            "the expired original pid stays inactive, got {agents:?}"
        );
        assert_eq!(
            roster_row(&agents, 222).status,
            "active",
            "the respawned pid is active, got {agents:?}"
        );
    }

    #[test]
    fn an_activity_records_last_seen_so_expiry_respects_the_window() {
        let mut graph = Graph::new();
        // The activity must record last_seen = t0 (not 0); otherwise the boundary
        // check below would see a huge age and expire prematurely.
        let t0: u64 = 5_000;
        let _ = graph.apply_clv_at(&activity_event("fn:src/x.rs:foo", "tdd-green", 333), t0);

        // Just after the activity, and again at exactly t0 + window, the process is
        // NOT strictly older than the window, so expiry is a no-op (empty vec).
        assert!(
            graph.expire_idle(t0 + 1).is_empty(),
            "just after the activity the process is not idle"
        );
        let boundary = graph.expire_idle(t0 + super::ROSTER_IDLE_MS);
        assert!(
            boundary.is_empty(),
            "at the window boundary the process is not yet idle, got {boundary:?}"
        );

        // One millisecond past the window it is strictly older and expires.
        let past = graph.expire_idle(t0 + super::ROSTER_IDLE_MS + 1);
        let agents = roster_agents(&past);
        assert_eq!(
            roster_row(&agents, 333).status,
            "inactive",
            "one ms past the window the process expires, got {agents:?}"
        );
    }

    // ---- P9-1: Graph::from_records rehydration constructor ----

    /// Models the load path for one file: parses `(path, src)` and strips each
    /// node's `child_ids` (the unpersisted field `load_nodes` returns as `[]`), so
    /// [`Graph::from_records`] must re-derive them from the `contains` edges (DD#7).
    fn loaded_records(path: &str, src: &str) -> (Vec<Node>, Vec<Edge>) {
        let parsed = parse_source(path, src);
        let nodes = parsed
            .nodes
            .into_iter()
            .map(|mut n| {
                n.child_ids = Vec::new();
                n
            })
            .collect();
        (nodes, parsed.edges)
    }

    /// Normalises a `snapshot` envelope for order-independent comparison: nodes
    /// sorted by id with each node's `child_ids` sorted, edges sorted by id.
    fn normalised_snapshot(env: &EventEnvelope) -> (Vec<Node>, Vec<Edge>) {
        let mut nodes = snapshot_nodes(env);
        for n in &mut nodes {
            n.child_ids.sort();
        }
        nodes.sort_by_key(|n| n.id.clone());
        let mut edges = snapshot_edges(env);
        edges.sort_by_key(|e| e.id.clone());
        (nodes, edges)
    }

    #[test]
    fn from_records_snapshot_equals_parsed_baseline() {
        // AC#3: a graph rebuilt from loaded records (child_ids stripped) has a
        // snapshot equal — order-independently, child_ids re-derived — to a graph
        // that PARSED the same file.
        let src = "fn alpha() { let x = 1; }\nfn beta() {}";

        let mut baseline = Graph::new();
        let _ = baseline.apply_parsed(parse_source("a.rs", src));

        let (nodes, edges) = loaded_records("a.rs", src);
        let rebuilt = Graph::from_records("sess-local", nodes, edges);

        assert_eq!(
            normalised_snapshot(&rebuilt.snapshot()),
            normalised_snapshot(&baseline.snapshot()),
            "rebuilt snapshot must equal the parsed-baseline snapshot"
        );
    }

    #[test]
    fn from_records_single_file_reparse_is_a_noop() {
        // AC#3 load-bearing proof: after rebuilding from loaded records, re-parsing
        // the SAME unchanged file emits no node.upsert/node.remove — proving the
        // child_ids re-derivation and the file_nodes/file_edges rebuild are correct.
        let src = "fn alpha() { let x = 1; }\nfn beta() {}";
        let (nodes, edges) = loaded_records("a.rs", src);
        let mut rebuilt = Graph::from_records("sess-local", nodes, edges);

        let diff = rebuilt.apply_parsed(parse_source("a.rs", src));
        assert!(
            diff.is_empty(),
            "reparse of an unchanged file must be a no-op, got {diff:?}"
        );
    }

    #[test]
    fn from_records_multi_file_reparse_one_leaves_other_intact() {
        // AC#4: rebuild from two files' records, re-parse ONE unchanged file → empty
        // diff AND the other file's nodes are not removed.
        let a_src = "fn alpha() { let x = 1; }";
        let b_src = "fn beta() {}";

        let (mut nodes, mut edges) = loaded_records("a.rs", a_src);
        let (b_nodes, b_edges) = loaded_records("b.rs", b_src);
        nodes.extend(b_nodes);
        edges.extend(b_edges);
        let mut rebuilt = Graph::from_records("sess-local", nodes, edges);

        let diff = rebuilt.apply_parsed(parse_source("a.rs", a_src));
        assert!(
            diff.is_empty(),
            "reparse of unchanged a.rs must be a no-op, got {diff:?}"
        );

        // b.rs's nodes must survive the a.rs reparse (no cross-file removal).
        let (b_children, _) = rebuilt.children_of("file:b.rs");
        assert!(
            b_children.iter().any(|n| n.id == "fn:b.rs:beta"),
            "b.rs function must not be removed by reparsing a.rs, got {b_children:?}"
        );
        let roots = snapshot_nodes(&rebuilt.snapshot());
        assert!(
            roots.iter().any(|n| n.id == "file:b.rs"),
            "b.rs file root must remain after reparsing a.rs, got {roots:?}"
        );
    }

    #[test]
    fn from_records_reparse_noop_is_edge_order_independent() {
        // Finding #2 regression: neither backend's `load_edges` has an `ORDER BY`, so
        // the loaded edge order is arbitrary. `from_records` must derive `child_ids`
        // canonically (sorted) — NOT in incidental edge order — or a differently-ordered
        // child list makes a rebuilt node fail the byte-equality no-op check on reparse.
        // The variable names (c, a, b) are deliberately out of source order so the sort
        // genuinely reorders them; reversing the loaded edges before rebuilding proves
        // the no-op holds regardless of edge order.
        let src = "fn alpha() { let c = 1; let a = 2; let b = 3; }\nfn beta() {}";
        let (nodes, mut edges) = loaded_records("a.rs", src);
        edges.reverse();
        let mut rebuilt = Graph::from_records("sess-local", nodes, edges);

        let diff = rebuilt.apply_parsed(parse_source("a.rs", src));
        assert!(
            diff.is_empty(),
            "reparse must be a no-op regardless of loaded edge order, got {diff:?}"
        );
    }

    #[test]
    fn from_records_with_agent_overlay_reparse_emits_no_spurious_removal() {
        // Finding #3 regression: `rebuild_file_tracking` must NOT absorb agent-layer
        // overlays — the live `apply_clv` path never file-tracks an `agent` vertex or an
        // `authored_by` edge. With both present in the loaded records (as a crash would
        // have persisted them), reparsing the agent-touched source file unchanged must
        // be a no-op — no spurious `node.remove`/`edge.remove` for the overlay.
        let src = "fn alpha() { let x = 1; }";
        let (mut nodes, mut edges) = loaded_records("a.rs", src);

        // The agent overlay `apply_clv` would have produced + persisted: a root agent
        // vertex and an `authored_by` edge from the touched function to it.
        let agent_vertex = agent_node_id("tdd-green");
        nodes.push(Node {
            id: agent_vertex.clone(),
            node_type: NodeType::Agent,
            label: "tdd-green".to_string(),
            parent_id: None,
            child_ids: Vec::new(),
            status: NodeStatus::Unknown,
            docs: None,
            signature: None,
            meta: None,
        });
        let authored_by_id = typed_edge_id("fn:a.rs:alpha", &agent_vertex, EdgeKind::AuthoredBy);
        edges.push(Edge {
            id: authored_by_id.clone(),
            source: "fn:a.rs:alpha".to_string(),
            target: agent_vertex.clone(),
            kind: EdgeKind::AuthoredBy,
            hot: false,
        });

        let mut rebuilt = Graph::from_records("sess-local", nodes, edges);

        let diff = rebuilt.apply_parsed(parse_source("a.rs", src));
        assert!(
            diff.is_empty(),
            "reparse of an agent-touched file must not disturb the agent overlay, got {diff:?}"
        );
        // The overlay must survive the reparse (not removed as a vanished file element).
        assert!(
            rebuilt.edges.contains_key(&authored_by_id),
            "the authored_by edge must survive the reparse"
        );
        assert!(
            rebuilt.nodes.contains_key(&agent_vertex),
            "the agent vertex must survive the reparse"
        );
    }

    // ---- P9-4: self-observability metrics (deterministic builder + latency map) ----

    /// Destructures a [`Payload::MetricsUpdate`] into
    /// `(node_count, edge_count, memory_bytes, events_per_sec_milli, parse_latency)`,
    /// panicking on any other payload — a test-only reader for the P9-4 metrics tests.
    fn metrics_fields(payload: &Payload) -> (u64, u64, u64, u64, &[FileParseLatency]) {
        match payload {
            Payload::MetricsUpdate {
                node_count,
                edge_count,
                memory_bytes,
                events_per_sec_milli,
                parse_latency,
                ..
            } => (
                *node_count,
                *edge_count,
                *memory_bytes,
                *events_per_sec_milli,
                parse_latency,
            ),
            other => panic!("expected MetricsUpdate payload, got {other:?}"),
        }
    }

    /// P9-4 AC: `metrics_payload` is a deterministic builder. Given a known graph and
    /// several recorded per-file latencies, it returns a [`Payload::MetricsUpdate`] whose
    /// `node_count`/`edge_count` mirror the live graph, whose `memory_bytes` is a pure
    /// function of state (identical across two calls), whose `events_per_sec_milli` is
    /// exactly the value passed in, and whose `parse_latency` vec is identical across two
    /// calls **including order** and ascending by `file_path`. Recording MORE THAN ONE
    /// path makes the ordering a real variable, so this pins the builder's
    /// `sort_by(file_path)` against `HashMap` iteration nondeterminism — dropping the sort
    /// would leave raw map order and fail the expected-order assertion. No timers, no
    /// sleeps — pure over graph state.
    #[test]
    fn metrics_payload_is_deterministic_and_reflects_the_graph() {
        let mut graph = graph_with_function(); // file:a.rs + fn:a.rs:f
                                               // Two+ distinct paths so the emitted order is not trivially a single element.
        graph.record_parse_latency("a.rs", 1234);
        graph.record_parse_latency("b.rs", 5678);
        graph.record_parse_latency("c.rs", 42);

        let first = graph.metrics_payload(4200);
        let second = graph.metrics_payload(4200);

        let (n1, e1, m1, eps1, lat1) = metrics_fields(&first);
        let (n2, e2, m2, eps2, lat2) = metrics_fields(&second);

        // Deterministic: identical graph state → identical scalar metrics every call.
        assert_eq!(
            m1, m2,
            "memory_bytes must be a pure function of graph state"
        );
        assert_eq!(
            (n1, e1, eps1),
            (n2, e2, eps2),
            "counts/throughput must be stable across calls"
        );

        // Counts mirror the live in-memory graph exactly.
        assert_eq!(
            n1,
            graph.nodes.len() as u64,
            "node_count must equal live nodes"
        );
        assert_eq!(
            e1,
            graph.edges.len() as u64,
            "edge_count must equal live edges"
        );
        // Throughput is exactly the argument (events/sec ×1000).
        assert_eq!(eps1, 4200, "events_per_sec_milli must echo the argument");

        // The full parse_latency vec is identical across two calls, order included …
        assert_eq!(
            lat1, lat2,
            "parse_latency must be byte-identical across calls, order included: {lat1:?} vs {lat2:?}"
        );
        // … and that order is exactly ascending by file_path. This locks the builder's
        // `sort_by(file_path)`: without it, `lat1` would be raw HashMap order and fail.
        let expected = [
            FileParseLatency {
                file_path: "a.rs".to_string(),
                duration_us: 1234,
            },
            FileParseLatency {
                file_path: "b.rs".to_string(),
                duration_us: 5678,
            },
            FileParseLatency {
                file_path: "c.rs".to_string(),
                duration_us: 42,
            },
        ];
        assert_eq!(
            lat1,
            expected.as_slice(),
            "parse_latency must be sorted ascending by file_path: {lat1:?}"
        );
    }

    /// P9-4: `metrics_envelope` is the retained public wrapper — it stamps a single clock
    /// reading into BOTH the envelope's `ts` and the payload's `ts`, and otherwise mirrors
    /// [`Graph::metrics_payload`]. Exercised directly so the retained seam stays covered
    /// even though the metrics emitter now assembles envelopes off-lock via
    /// [`Graph::metrics_snapshot`] + [`MetricsSnapshot::into_envelope`].
    #[test]
    fn metrics_envelope_stamps_one_timestamp_into_envelope_and_payload() {
        let mut graph = graph_with_function(); // file:a.rs + fn:a.rs:f
        graph.record_parse_latency("a.rs", 99);

        let env = graph.metrics_envelope(1500);
        assert_eq!(env.event_type, EventType::MetricsUpdate);
        assert!(!env.ts.is_empty(), "the envelope ts must be stamped");
        match &env.payload {
            Payload::MetricsUpdate {
                ts,
                session_id,
                node_count,
                events_per_sec_milli,
                ..
            } => {
                assert_eq!(*ts, env.ts, "payload ts must equal the envelope ts");
                assert_eq!(
                    *session_id, env.session_id,
                    "payload session must match the envelope session"
                );
                assert_eq!(
                    *node_count,
                    graph.nodes.len() as u64,
                    "node_count must mirror the live graph"
                );
                assert_eq!(
                    *events_per_sec_milli, 1500,
                    "events_per_sec_milli must echo the argument"
                );
            }
            other => panic!("expected metrics.update, got {other:?}"),
        }
    }

    /// P9-4 AC: `record_parse_latency` keeps the most-recent duration per repo-relative
    /// path and is bounded by the distinct-file count. Re-recording a path overwrites
    /// (the map stays size 1, latest value wins); a second distinct path grows it to 2.
    #[test]
    fn record_parse_latency_is_bounded_and_overwrites_per_path() {
        let mut graph = Graph::new();
        graph.record_parse_latency("a.rs", 100);
        graph.record_parse_latency("a.rs", 250); // same path → overwrite, not append

        let payload = graph.metrics_payload(0);
        let (.., latency) = metrics_fields(&payload);
        assert_eq!(
            latency.len(),
            1,
            "re-parsing one path stays a single entry: {latency:?}"
        );
        assert_eq!(
            latency[0].duration_us, 250,
            "the latest recorded duration must win: {latency:?}"
        );

        graph.record_parse_latency("b.rs", 500); // a distinct path → bounded growth to 2
        let payload = graph.metrics_payload(0);
        let (.., latency) = metrics_fields(&payload);
        assert_eq!(
            latency.len(),
            2,
            "two distinct paths → two entries: {latency:?}"
        );
        assert!(latency
            .iter()
            .any(|f| f.file_path == "a.rs" && f.duration_us == 250));
        assert!(latency
            .iter()
            .any(|f| f.file_path == "b.rs" && f.duration_us == 500));
    }
}
