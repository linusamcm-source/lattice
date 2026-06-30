/**
 * Typed WebSocket client and reactive graph store for the Phase 0 walking skeleton.
 *
 * Event flow:
 * 1. {@link connect} opens a socket and registers an `onmessage` handler.
 * 2. Each raw message is run through {@link parseEnvelope}, which validates the
 *    discriminant and returns a typed {@link EventEnvelope} (or `null`).
 * 3. Valid envelopes are folded into {@link graphStore} via the pure reducer
 *    {@link applyEvent}, so the reducer is unit-testable without a real socket.
 * 4. Components subscribe to the derived {@link nodes} / {@link edges} stores.
 *
 * `snapshot` replaces the whole graph; `node.upsert`/`edge.upsert` insert-or-update
 * by id; `node.remove`/`edge.remove` delete by id; the Phase-1 `subtree` reply
 * (the lazy `expand` answer) merges a parent's direct children in by id;
 * `test.result`/`status.update` recolour a node; the Phase-6 `hot_edge` event
 * toggles an edge's `hot` flag. Subtrees are fetched only via
 * {@link requestExpand} and discarded via {@link collapse}.
 * Auto-reconnect is deliberately out of scope for Phase 0 (it lands in Phase 9).
 *
 * @module
 */

import { writable, derived, type Readable, type Writable } from 'svelte/store';
import type { Node, Edge, EventEnvelope, EventType, NodeStatus, TestOutcome } from './types';

/**
 * The reducer state: the current graph indexed by id for O(1) upsert/remove.
 * Treated as immutable — {@link applyEvent} returns fresh maps and never mutates
 * the input, which keeps store updates predictable and the reducer pure.
 */
export interface GraphState {
	nodes: Map<string, Node>;
	edges: Map<string, Edge>;
}

/** Construct an empty {@link GraphState}. */
export function initialState(): GraphState {
	return { nodes: new Map(), edges: new Map() };
}

/**
 * Maps a CLV {@link TestOutcome} onto the {@link NodeStatus} colour the graph
 * shows: `fail`->`failing`, `pass`->`passing`, `skip`->`stale`, `running`->`running`.
 */
const TEST_OUTCOME_STATUS: Record<TestOutcome, NodeStatus> = {
	pass: 'passing',
	fail: 'failing',
	skip: 'stale',
	running: 'running'
};

/**
 * Pure reducer: fold one {@link EventEnvelope} into the graph state, returning a
 * new {@link GraphState}. The input is never mutated. Node-targeted
 * (`test.result`/`status.update`) and edge-targeted (`hot_edge`) events whose
 * target id is absent are no-ops that return the same state. Unknown event types
 * (which the typed union already excludes) leave the state unchanged.
 *
 * @param state - the current graph state.
 * @param envelope - a validated CLV envelope.
 * @returns the next graph state.
 */
export function applyEvent(state: GraphState, envelope: EventEnvelope): GraphState {
	switch (envelope.type) {
		case 'snapshot': {
			const nextNodes = new Map(envelope.payload.nodes.map((n) => [n.id, n]));
			const nextEdges = new Map(envelope.payload.edges.map((e) => [e.id, e]));
			return { nodes: nextNodes, edges: nextEdges };
		}
		case 'node.upsert': {
			const nextNodes = new Map(state.nodes);
			nextNodes.set(envelope.payload.node.id, envelope.payload.node);
			return { nodes: nextNodes, edges: state.edges };
		}
		case 'node.remove': {
			const nextNodes = new Map(state.nodes);
			nextNodes.delete(envelope.payload.id);
			return { nodes: nextNodes, edges: state.edges };
		}
		case 'edge.upsert': {
			const nextEdges = new Map(state.edges);
			nextEdges.set(envelope.payload.edge.id, envelope.payload.edge);
			return { nodes: state.nodes, edges: nextEdges };
		}
		case 'edge.remove': {
			const nextEdges = new Map(state.edges);
			nextEdges.delete(envelope.payload.id);
			return { nodes: state.nodes, edges: nextEdges };
		}
		case 'subtree': {
			// Lazy `expand` reply: merge the parent's direct children into the
			// existing graph by id (existing entries preserved, children
			// inserted-or-updated). Never a whole-graph replacement.
			const nextNodes = new Map(state.nodes);
			for (const node of envelope.payload.nodes) nextNodes.set(node.id, node);
			const nextEdges = new Map(state.edges);
			for (const edge of envelope.payload.edges) nextEdges.set(edge.id, edge);
			return { nodes: nextNodes, edges: nextEdges };
		}
		case 'test.result': {
			// Fold a test outcome onto the target node's colour; an absent node id is a
			// no-op (no phantom node, immutable state preserved).
			const node = state.nodes.get(envelope.payload.nodeId);
			if (!node) return state;
			const nextNodes = new Map(state.nodes);
			nextNodes.set(node.id, { ...node, status: TEST_OUTCOME_STATUS[envelope.payload.outcome] });
			return { nodes: nextNodes, edges: state.edges };
		}
		case 'status.update': {
			// Apply an explicit status to the target node; an absent id is a no-op.
			const node = state.nodes.get(envelope.payload.nodeId);
			if (!node) return state;
			const nextNodes = new Map(state.nodes);
			nextNodes.set(node.id, { ...node, status: envelope.payload.status });
			return { nodes: nextNodes, edges: state.edges };
		}
		case 'hot_edge': {
			// Toggle the target edge's `hot` flag (`enter`->true, `exit`->false); an
			// absent edge id is a no-op (no phantom edge, immutable state preserved).
			const edge = state.edges.get(envelope.payload.edgeId);
			if (!edge) return state;
			const nextEdges = new Map(state.edges);
			nextEdges.set(edge.id, { ...edge, hot: envelope.payload.state === 'enter' });
			return { nodes: state.nodes, edges: nextEdges };
		}
	}
}

const KNOWN_EVENT_TYPES: ReadonlySet<EventType> = new Set([
	'snapshot',
	'node.upsert',
	'node.remove',
	'edge.upsert',
	'edge.remove',
	'subtree',
	'test.result',
	'status.update',
	'hot_edge'
]);

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === 'object' && value !== null;
}

/**
 * Shape-check the payload for the targeted event types. `test.result` and
 * `status.update` must carry a string `nodeId` (plus a string `outcome`/`status`
 * respectively); `hot_edge` must carry a string `edgeId` and an `enter`/`exit`
 * `state`; all other types are accepted as-is. Never widens to `any`.
 */
function isValidPayload(type: EventType, payload: Record<string, unknown>): boolean {
	switch (type) {
		case 'test.result':
			return typeof payload.nodeId === 'string' && typeof payload.outcome === 'string';
		case 'status.update':
			return typeof payload.nodeId === 'string' && typeof payload.status === 'string';
		case 'hot_edge':
			return (
				typeof payload.edgeId === 'string' &&
				(payload.state === 'enter' || payload.state === 'exit')
			);
		default:
			return true;
	}
}

/**
 * Validate an untrusted WebSocket message into a typed {@link EventEnvelope}.
 *
 * Accepts either a raw JSON string (it is parsed) or an already-decoded value.
 * Returns `null` — never throws and never widens to `any` — when the input is
 * not JSON, is not an object, or carries an unrecognised `type` discriminant.
 *
 * @param raw - the socket payload (`string`) or a decoded value.
 * @returns a typed envelope, or `null` if it fails validation.
 */
export function parseEnvelope(raw: unknown): EventEnvelope | null {
	let value: unknown = raw;
	if (typeof raw === 'string') {
		try {
			value = JSON.parse(raw);
		} catch {
			return null;
		}
	}
	if (!isRecord(value)) return null;
	const type = value.type;
	if (typeof type !== 'string' || !KNOWN_EVENT_TYPES.has(type as EventType)) return null;
	if (!isRecord(value.payload)) return null;
	if (!isValidPayload(type as EventType, value.payload)) return null;
	return value as unknown as EventEnvelope;
}

/**
 * The authoritative graph store. Components should prefer the derived
 * {@link nodes} / {@link edges} views; this is exported for test setup/reset.
 */
export const graphStore: Writable<GraphState> = writable(initialState());

/** Derived list of all current nodes (insertion order from the underlying map). */
export const nodes: Readable<Node[]> = derived(graphStore, ($g) => Array.from($g.nodes.values()));

/** Derived list of all current edges. */
export const edges: Readable<Edge[]> = derived(graphStore, ($g) => Array.from($g.edges.values()));

/**
 * Fold a validated envelope into {@link graphStore} via {@link applyEvent}.
 * This is the single mutation entry point shared by the socket and by tests.
 *
 * @param envelope - a typed CLV envelope.
 */
export function ingest(envelope: EventEnvelope): void {
	graphStore.update((state) => applyEvent(state, envelope));
}

/** A live WebSocket connection and its teardown handle. */
export interface WsClient {
	socket: WebSocket;
	close: () => void;
}

/**
 * Open a WebSocket to `url` and stream incoming CLV envelopes into the graph
 * store. Each message is validated by {@link parseEnvelope}; malformed messages
 * are ignored. No reconnection logic (Phase 9).
 *
 * @param url - the WebSocket URL (e.g. `ws://localhost:7000`).
 * @returns the socket plus a `close()` handle.
 */
export function connect(url: string): WsClient {
	const socket = new WebSocket(url);
	socket.addEventListener('message', (event: MessageEvent) => {
		const data: unknown = event.data;
		const envelope = parseEnvelope(data);
		if (envelope) ingest(envelope);
	});
	return { socket, close: () => socket.close() };
}

/**
 * Send the Phase-1 lazy-load `expand` request for `nodeId` over `socket`.
 *
 * Mirrors the `{"type":"snapshot"}` resync frame: the serialized payload is
 * exactly `{"type":"expand","nodeId":"<nodeId>"}` (keys `type` then `nodeId`),
 * which the backend answers with a `subtree` envelope carrying that node's
 * direct children. Children are fetched only on this explicit request — the
 * client never pre-fetches a subtree.
 *
 * @param socket - the live WebSocket connection.
 * @param nodeId - the id of the node whose children to load.
 */
export function requestExpand(socket: WebSocket, nodeId: string): void {
	socket.send(JSON.stringify({ type: 'expand', nodeId }));
}

/**
 * Pure collapse-discard: return a new {@link GraphState} with `nodeId`'s
 * **transitive** descendants removed, bounding client memory after a collapse.
 *
 * A node is a descendant when it is reachable by following `parentId` edges
 * down from `nodeId`; every such node is dropped, along with any edge whose
 * `source` or `target` was dropped. `nodeId` itself and all unrelated nodes are
 * preserved. The input state is never mutated.
 *
 * @param state - the current graph state.
 * @param nodeId - the node to collapse (kept; its descendants are discarded).
 * @returns the next graph state without `nodeId`'s descendants.
 */
export function collapse(state: GraphState, nodeId: string): GraphState {
	const removed = new Set<string>();
	const queue: string[] = [nodeId];
	while (queue.length > 0) {
		const parent = queue.shift() as string;
		for (const node of state.nodes.values()) {
			if (node.parentId === parent && !removed.has(node.id)) {
				removed.add(node.id);
				queue.push(node.id);
			}
		}
	}
	const nextNodes = new Map<string, Node>();
	for (const [id, node] of state.nodes) {
		if (!removed.has(id)) nextNodes.set(id, node);
	}
	const nextEdges = new Map<string, Edge>();
	for (const [id, edge] of state.edges) {
		if (!removed.has(edge.source) && !removed.has(edge.target)) nextEdges.set(id, edge);
	}
	return { nodes: nextNodes, edges: nextEdges };
}
