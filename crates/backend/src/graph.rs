//! In-memory CLV graph and the diff that turns a re-parse into patch events.
//!
//! [`Graph`] is the Phase-0 source of truth for the live structural graph: it
//! holds the current [`Node`](crate::wire::Node)s and
//! [`Edge`](crate::wire::Edge)s keyed by their deterministic ids and turns
//! parser output into the [`EventEnvelope`](crate::wire::EventEnvelope) stream a
//! WebSocket client consumes.
//!
//! Two write paths exist. [`Graph::upsert_node`]/[`Graph::upsert_edge`] are raw
//! insert-or-update-by-id mutators. [`Graph::apply_parsed`] is the higher-level
//! path: it diffs a file's previous contribution against a fresh
//! [`ParsedFile`](crate::parser::ParsedFile) and emits `node.upsert`/`edge.upsert`
//! for added-or-changed elements and `node.remove`/`edge.remove` for elements that
//! vanished from that file, so re-applying an identical parse is a no-op.
//! [`Graph::snapshot`] renders the whole graph as one `snapshot` envelope for a
//! freshly connected client.
//!
//! Per `AGENT_PROTOCOL.md` §6 this is panic-free: a clock error or a parse with no
//! `file` node degrades to a safe default rather than unwrapping.

use std::collections::HashMap;

use crate::parser::ParsedFile;
use crate::wire::{Edge, EventEnvelope, EventType, Node, NodeType, Payload};

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

    /// Renders the whole current graph as one `snapshot` [`EventEnvelope`].
    ///
    /// The payload is [`Payload::Snapshot`] carrying every current node and edge;
    /// the WebSocket server sends this to each client on connect and on resync.
    pub fn snapshot(&self) -> EventEnvelope {
        let nodes = self.nodes.values().cloned().collect();
        let edges = self.edges.values().cloned().collect();
        self.envelope(EventType::Snapshot, Payload::Snapshot { nodes, edges })
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

    /// Wraps `payload` in an [`EventEnvelope`] stamped with this graph's session.
    fn envelope(&self, event_type: EventType, payload: Payload) -> EventEnvelope {
        EventEnvelope {
            v: PROTOCOL_VERSION,
            ts: rfc3339_now(),
            session_id: self.session_id.clone(),
            event_type,
            payload,
        }
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
    use crate::parser::parse_rust_source;
    use crate::wire::{
        node_id, Edge, EventEnvelope, EventType, Node, NodeStatus, NodeType, Payload,
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
    fn snapshot_contains_all_current_nodes_and_edges() {
        let mut graph = Graph::new();
        let parsed = parse_rust_source("src/x.rs", "fn foo() {}");
        let _ = graph.apply_parsed(parsed.clone());

        let env = graph.snapshot();
        assert_eq!(env.event_type, EventType::Snapshot);
        let nodes = snapshot_nodes(&env);
        let edges = snapshot_edges(&env);
        for want in &parsed.nodes {
            assert!(
                nodes.iter().any(|n| n.id == want.id),
                "snapshot missing node {}",
                want.id
            );
        }
        for want in &parsed.edges {
            assert!(
                edges.iter().any(|e| e.id == want.id),
                "snapshot missing edge {}",
                want.id
            );
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
}
