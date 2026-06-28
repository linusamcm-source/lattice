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
 * by id; `node.remove`/`edge.remove` delete by id. Auto-reconnect is deliberately
 * out of scope for Phase 0 (it lands in Phase 9).
 *
 * @module
 */

import { writable, derived, type Readable, type Writable } from 'svelte/store';
import type { Node, Edge, EventEnvelope, EventType } from './types';

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
 * Pure reducer: fold one {@link EventEnvelope} into the graph state, returning a
 * new {@link GraphState}. The input is never mutated. Unknown event types (which
 * the typed union already excludes) leave the state unchanged.
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
	}
}

const KNOWN_EVENT_TYPES: ReadonlySet<EventType> = new Set([
	'snapshot',
	'node.upsert',
	'node.remove',
	'edge.upsert',
	'edge.remove'
]);

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === 'object' && value !== null;
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
