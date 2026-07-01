/**
 * Typed WebSocket client and reactive graph store for the Phase 0 walking skeleton.
 *
 * Event flow:
 * 1. {@link connect} opens a socket and registers `open`/`message`/`close`/`error`
 *    listeners, so it can drive the {@link connectionStatus} lifecycle and
 *    auto-reconnect a dropped socket.
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
 * toggles an edge's `hot` flag. The Phase-8 agent layer rides the same store: an
 * `agent.roster` rebuilds the {@link GraphState.agents} roster (keyed by
 * `processId`) while agent structure (`agent` nodes, `authored_by` edges) folds
 * through the existing `node.upsert`/`edge.upsert` channels. The Phase-9
 * `metrics.update` event stores the latest self-observability snapshot in
 * {@link GraphState.metrics}, surfaced via the derived {@link metrics} store that
 * `MetricsPanel` renders. Subtrees are fetched only via {@link requestExpand} and
 * discarded via {@link collapse}.
 *
 * The Phase-9 socket lifecycle: {@link connect} auto-reconnects with exponential
 * backoff (cap + jitter) and, on every *re*-open (not the first — the server
 * already pushes a root snapshot on connect), resyncs by sending
 * `{"type":"snapshot"}` first, then one `expand` per still-open node
 * ({@link ConnectOptions.getExpandedNodes}), so the client converges back to the
 * server's state after a drop. {@link connectionStatus} exposes the lifecycle
 * (`connecting`/`open`/`reconnecting`/`closed`) for the UI's reconnecting badge, and
 * the returned {@link WsClient} hands back stable `requestExpand`/`send` handles
 * that always target the current live socket — never a stale one held from before a
 * drop. Every send is gated on `readyState === OPEN` ({@link safeSend}) so a click
 * during the reconnect window neither throws nor writes to a dead socket; the
 * dropped `expand` is replayed by the next re-open's resync. Each socket's four
 * lifecycle listeners are bound under a per-socket `AbortController`, aborted on
 * swap, so a stale socket's late terminal event can't drive a second reconnect.
 *
 * @module
 */

import { writable, derived, type Readable, type Writable } from 'svelte/store';
import type {
	Node,
	Edge,
	AgentInfo,
	EventEnvelope,
	EventType,
	MetricsUpdatePayload,
	NodeStatus,
	TestOutcome
} from './types';

/**
 * The reducer state: the current graph indexed by id for O(1) upsert/remove.
 * Treated as immutable — {@link applyEvent} returns fresh maps and never mutates
 * the input, which keeps store updates predictable and the reducer pure.
 */
export interface GraphState {
	nodes: Map<string, Node>;
	edges: Map<string, Edge>;
	/** The Phase-8 agent roster, keyed by stringified `processId`. */
	agents: Map<string, AgentInfo>;
	/** The latest Phase-9 `metrics.update` snapshot, or `null` before the first one. */
	metrics: MetricsUpdatePayload | null;
}

/** Construct an empty {@link GraphState}. */
export function initialState(): GraphState {
	return { nodes: new Map(), edges: new Map(), agents: new Map(), metrics: null };
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
 * target id is absent are no-ops that return the same state. `agent.roster`
 * replaces the `agents` roster (keyed by `processId`); `agent.activity` carries no
 * graph delta and is a no-op; `metrics.update` stores the latest self-observability
 * snapshot in `metrics`. Unknown event types (which the typed union already
 * excludes) leave the state unchanged.
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
			return { nodes: nextNodes, edges: nextEdges, agents: state.agents, metrics: state.metrics };
		}
		case 'node.upsert': {
			const nextNodes = new Map(state.nodes);
			nextNodes.set(envelope.payload.node.id, envelope.payload.node);
			return { nodes: nextNodes, edges: state.edges, agents: state.agents, metrics: state.metrics };
		}
		case 'node.remove': {
			const nextNodes = new Map(state.nodes);
			nextNodes.delete(envelope.payload.id);
			return { nodes: nextNodes, edges: state.edges, agents: state.agents, metrics: state.metrics };
		}
		case 'edge.upsert': {
			const nextEdges = new Map(state.edges);
			nextEdges.set(envelope.payload.edge.id, envelope.payload.edge);
			return { nodes: state.nodes, edges: nextEdges, agents: state.agents, metrics: state.metrics };
		}
		case 'edge.remove': {
			const nextEdges = new Map(state.edges);
			nextEdges.delete(envelope.payload.id);
			return { nodes: state.nodes, edges: nextEdges, agents: state.agents, metrics: state.metrics };
		}
		case 'subtree': {
			// Lazy `expand` reply: merge the parent's direct children into the
			// existing graph by id (existing entries preserved, children
			// inserted-or-updated). Never a whole-graph replacement.
			const nextNodes = new Map(state.nodes);
			for (const node of envelope.payload.nodes) nextNodes.set(node.id, node);
			const nextEdges = new Map(state.edges);
			for (const edge of envelope.payload.edges) nextEdges.set(edge.id, edge);
			return { nodes: nextNodes, edges: nextEdges, agents: state.agents, metrics: state.metrics };
		}
		case 'test.result': {
			// Fold a test outcome onto the target node's colour; an absent node id is a
			// no-op (no phantom node, immutable state preserved).
			const node = state.nodes.get(envelope.payload.nodeId);
			if (!node) return state;
			const nextNodes = new Map(state.nodes);
			nextNodes.set(node.id, { ...node, status: TEST_OUTCOME_STATUS[envelope.payload.outcome] });
			return { nodes: nextNodes, edges: state.edges, agents: state.agents, metrics: state.metrics };
		}
		case 'status.update': {
			// Apply an explicit status to the target node; an absent id is a no-op.
			const node = state.nodes.get(envelope.payload.nodeId);
			if (!node) return state;
			const nextNodes = new Map(state.nodes);
			nextNodes.set(node.id, { ...node, status: envelope.payload.status });
			return { nodes: nextNodes, edges: state.edges, agents: state.agents, metrics: state.metrics };
		}
		case 'hot_edge': {
			// Toggle the target edge's `hot` flag (`enter`->true, `exit`->false); an
			// absent edge id is a no-op (no phantom edge, immutable state preserved).
			const edge = state.edges.get(envelope.payload.edgeId);
			if (!edge) return state;
			const nextEdges = new Map(state.edges);
			nextEdges.set(edge.id, { ...edge, hot: envelope.payload.state === 'enter' });
			return { nodes: state.nodes, edges: nextEdges, agents: state.agents, metrics: state.metrics };
		}
		case 'agent.roster': {
			// Rebuild the agent roster from the payload, keyed by stringified
			// `processId`. Returns a fresh map and never mutates the input roster, so
			// the reducer stays pure. Node/edge maps pass through untouched — agent
			// structure rides the `node.upsert`/`edge.upsert` channels instead. Each
			// row is rebuilt from only the known fields so no extra untrusted
			// own-property from the wire reaches reactive state.
			const nextAgents = new Map<string, AgentInfo>();
			for (const agent of envelope.payload.agents) {
				const info: AgentInfo = {
					processId: agent.processId,
					agentId: agent.agentId,
					agentType: agent.agentType,
					color: agent.color,
					status: agent.status
				};
				if (agent.protocolVersion !== undefined) info.protocolVersion = agent.protocolVersion;
				nextAgents.set(String(agent.processId), info);
			}
			return { nodes: state.nodes, edges: state.edges, agents: nextAgents, metrics: state.metrics };
		}
		case 'agent.activity':
			// An activity carries no roster delta (node attribution rides the
			// structural `node.upsert`/`edge.upsert`/`agent.roster` channels), so the
			// graph is unchanged — a no-op that returns the same state, matching the
			// other no-op branches.
			return state;
		case 'metrics.update':
			// Store the latest self-observability snapshot; the node/edge/agent maps
			// pass through untouched. A fresh state object is returned (input never
			// mutated) so the reducer stays pure and the derived `metrics` store fires.
			return {
				nodes: state.nodes,
				edges: state.edges,
				agents: state.agents,
				metrics: envelope.payload
			};
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
	'hot_edge',
	'agent.roster',
	'agent.activity',
	'metrics.update'
]);

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === 'object' && value !== null;
}

/**
 * Shape-check a single {@link AgentInfo} roster row: every required key must be
 * present with its wire type (camelCase, mirroring the Rust `AgentInfo` struct).
 */
function isAgentInfo(value: unknown): value is AgentInfo {
	return (
		isRecord(value) &&
		typeof value.processId === 'number' &&
		typeof value.agentId === 'string' &&
		typeof value.agentType === 'string' &&
		typeof value.color === 'string' &&
		typeof value.status === 'string'
	);
}

/**
 * Shape-check a single `metrics.update` parse-latency row: a string `filePath`
 * and a numeric `durationUs` (mirroring the Rust `FileParseLatency` struct).
 */
function isFileParseLatency(value: unknown): boolean {
	return (
		isRecord(value) && typeof value.filePath === 'string' && typeof value.durationUs === 'number'
	);
}

/**
 * Shape-check the payload for the targeted event types. `test.result` and
 * `status.update` must carry a string `nodeId` (plus a string `outcome`/`status`
 * respectively); `hot_edge` must carry a string `edgeId` and an `enter`/`exit`
 * `state`; `agent.roster` must carry an `agents` array of well-formed
 * {@link AgentInfo} rows; `agent.activity` must carry string `agentId`/`action`/
 * `nodeId`; `metrics.update` must carry string `sessionId`/`ts`, numeric
 * `nodeCount`/`edgeCount`/`memoryBytes`/`eventsPerSecMilli`, and a `parseLatency`
 * array of well-formed `{filePath,durationUs}` rows; all other types are accepted
 * as-is. Never widens to `any`.
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
		case 'agent.roster':
			return Array.isArray(payload.agents) && payload.agents.every(isAgentInfo);
		case 'agent.activity':
			return (
				typeof payload.agentId === 'string' &&
				typeof payload.action === 'string' &&
				typeof payload.nodeId === 'string'
			);
		case 'metrics.update':
			return (
				typeof payload.sessionId === 'string' &&
				typeof payload.ts === 'string' &&
				typeof payload.nodeCount === 'number' &&
				typeof payload.edgeCount === 'number' &&
				typeof payload.memoryBytes === 'number' &&
				typeof payload.eventsPerSecMilli === 'number' &&
				Array.isArray(payload.parseLatency) &&
				payload.parseLatency.every(isFileParseLatency)
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

/** Derived list of the current Phase-8 agent roster (insertion order). */
export const agents: Readable<AgentInfo[]> = derived(graphStore, ($g) =>
	Array.from($g.agents.values())
);

/**
 * Derived Phase-9 self-observability snapshot — the latest `metrics.update`
 * payload, or `null` before the first one arrives. `MetricsPanel` binds this.
 */
export const metrics: Readable<MetricsUpdatePayload | null> = derived(
	graphStore,
	($g) => $g.metrics
);

/**
 * Fold a validated envelope into {@link graphStore} via {@link applyEvent}.
 * This is the single mutation entry point shared by the socket and by tests.
 *
 * @param envelope - a typed CLV envelope.
 */
export function ingest(envelope: EventEnvelope): void {
	graphStore.update((state) => applyEvent(state, envelope));
}

/**
 * The socket lifecycle the UI badge reflects:
 * - `connecting` — a socket is opening for the first time (pre-`open`);
 * - `open` — the live socket is up and streaming;
 * - `reconnecting` — the socket dropped and a backed-off retry is scheduled;
 * - `closed` — {@link WsClient.close} tore the client down intentionally.
 */
export type ConnectionStatus = 'connecting' | 'open' | 'reconnecting' | 'closed';

/**
 * Reactive connection lifecycle, driven by {@link connect} and read by the UI to
 * show a "reconnecting" indicator while the socket is down. A module singleton so
 * any component can subscribe without threading a prop.
 */
export const connectionStatus: Writable<ConnectionStatus> = writable('connecting');

/**
 * A live WebSocket connection plus the stable handles callers use to talk to it.
 * `socket` is swapped to the current live socket on every reconnect, and
 * {@link WsClient.requestExpand}/{@link WsClient.send} always target that current
 * socket — never a stale one held from before a drop.
 */
export interface WsClient {
	/** The current live socket (reassigned on each reconnect). */
	socket: WebSocket;
	/** Send a lazy `expand` request over the current live socket. */
	requestExpand: (nodeId: string) => void;
	/** Send a raw frame over the current live socket. */
	send: (data: string) => void;
	/** Tear the client down intentionally; stops reconnecting. */
	close: () => void;
}

/** Options for {@link connect}. */
export interface ConnectOptions {
	/**
	 * Supplies the set of currently-open node ids, re-read **fresh on every
	 * re-open** so a node collapsed between drops is not re-expanded. This is how
	 * the render-side `expanded` set (owned by `Graph.svelte`) crosses the layer
	 * into the reconnect resync.
	 */
	getExpandedNodes?: () => Iterable<string>;
	/** Exponential-backoff schedule; a sensible default is used when omitted. */
	backoff?: { baseMs: number; maxMs: number; jitter?: () => number };
}

/** Default backoff: 0.5s base, 15s cap, up to 250ms of jitter to de-sync retries. */
const DEFAULT_BACKOFF = { baseMs: 500, maxMs: 15_000, jitter: () => Math.random() * 250 };

/** The canonical resync frame sent first on every (re-)open. */
const SNAPSHOT_REQUEST = JSON.stringify({ type: 'snapshot' });

/**
 * Open a resilient WebSocket to `url` and stream incoming CLV envelopes into the
 * graph store. Each message is validated by {@link parseEnvelope} (malformed
 * messages are ignored) and folded through the single {@link ingest} entry point.
 *
 * Lifecycle: {@link connectionStatus} goes `connecting` → `open` on the socket's
 * `open` event → `reconnecting` when it drops (`close`/`error`) → `open` again on
 * recovery → `closed` when {@link WsClient.close} is called. A drop schedules one
 * reconnect via `setTimeout` after {@link connect~backoffDelay}; a successful
 * re-open **resets** the attempt counter. On every (re-)open the client sends the
 * `{"type":"snapshot"}` resync frame **first**, then one `expand` per id from
 * {@link ConnectOptions.getExpandedNodes}, so the graph converges back to the
 * server's state (BUILD_PLAN cross-cutting).
 *
 * @param url - the WebSocket URL (e.g. `ws://127.0.0.1:7000`).
 * @param options - optional open-node supplier and backoff schedule.
 * @returns a {@link WsClient} whose `socket` and send handles always target the
 *   current live socket.
 */
export function connect(url: string, options?: ConnectOptions): WsClient {
	const backoff = options?.backoff ?? DEFAULT_BACKOFF;
	let socket: WebSocket;
	let attempt = 0;
	let intentionalClose = false;
	let reconnectPending = false;
	let hasConnectedOnce = false;
	let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
	/** Aborts the current socket's lifecycle listeners when the socket is swapped. */
	let socketAbort: AbortController | undefined;

	/**
	 * Delay before the next reconnect: the exponential term
	 * `baseMs * 2 ** (attempt - 1)` is capped at `maxMs` **before** jitter is
	 * added, so a zero-jitter schedule is exactly base, 2×, 4×, … up to the cap.
	 */
	function backoffDelay(): number {
		const capped = Math.min(backoff.maxMs, backoff.baseMs * 2 ** (attempt - 1));
		return capped + (backoff.jitter ? backoff.jitter() : 0);
	}

	/** Resync a freshly-opened socket: snapshot first, then re-expand each open node. */
	function resync(): void {
		safeSend(socket, SNAPSHOT_REQUEST);
		const open = options?.getExpandedNodes?.();
		if (open) for (const id of open) requestExpand(socket, id);
	}

	function handleOpen(): void {
		attempt = 0; // recovery resets the backoff schedule
		connectionStatus.set('open');
		// Skip the redundant resync on the FIRST open: the server already pushes a
		// root snapshot on connect. Only re-opens replay snapshot + re-expand.
		if (hasConnectedOnce) resync();
		else hasConnectedOnce = true;
	}

	function handleMessage(event: MessageEvent): void {
		const data: unknown = event.data;
		const envelope = parseEnvelope(data);
		if (envelope) ingest(envelope);
	}

	/**
	 * A socket drop (`close`/`error`, not an intentional `close()`) schedules one
	 * backed-off reconnect. `reconnectPending` dedupes the `error`-then-`close`
	 * pair browsers fire, so a single drop advances the backoff only once.
	 */
	function handleDrop(): void {
		if (intentionalClose || reconnectPending) return;
		reconnectPending = true;
		attempt += 1;
		connectionStatus.set('reconnecting');
		reconnectTimer = setTimeout(() => {
			reconnectPending = false;
			openSocket();
		}, backoffDelay());
	}

	/**
	 * Open a fresh socket and bind the lifecycle listeners onto it under a
	 * per-socket {@link AbortController}. The outgoing socket's controller is
	 * aborted first, detaching its listeners so a stale socket's late
	 * `close`/`error` can't resolve the shared {@link handleDrop} and drive a
	 * second reconnect.
	 */
	function openSocket(): void {
		socketAbort?.abort();
		const controller = new AbortController();
		socketAbort = controller;
		const { signal } = controller;
		socket = new WebSocket(url);
		socket.addEventListener('open', handleOpen, { signal });
		socket.addEventListener('message', handleMessage, { signal });
		socket.addEventListener('close', handleDrop, { signal });
		socket.addEventListener('error', handleDrop, { signal });
	}

	connectionStatus.set('connecting');
	openSocket();

	return {
		get socket() {
			return socket;
		},
		requestExpand: (nodeId: string) => requestExpand(socket, nodeId),
		send: (data: string) => safeSend(socket, data),
		close: () => {
			intentionalClose = true;
			clearTimeout(reconnectTimer);
			socketAbort?.abort();
			connectionStatus.set('closed');
			socket.close();
		}
	};
}

/**
 * Send `data` over `socket` only while it is `OPEN`. During the reconnect window a
 * socket is either `CLOSED` (a `send` is silently dropped) or `CONNECTING` (a
 * `send` throws `InvalidStateError`), so an unguarded send from a user expand click
 * mid-reconnect surfaces an uncaught `DOMException`. Gating here makes that click a
 * no-op; the id it targeted is already in the render-side open set, so the next
 * re-open's {@link connect~resync} replays the `expand`.
 *
 * @param socket - the (possibly not-yet-open) WebSocket.
 * @param data - the frame to send when the socket is open.
 */
function safeSend(socket: WebSocket, data: string): void {
	if (socket.readyState === WebSocket.OPEN) socket.send(data);
}

/**
 * Send the Phase-1 lazy-load `expand` request for `nodeId` over `socket`.
 *
 * Mirrors the `{"type":"snapshot"}` resync frame: the serialized payload is
 * exactly `{"type":"expand","nodeId":"<nodeId>"}` (keys `type` then `nodeId`),
 * which the backend answers with a `subtree` envelope carrying that node's
 * direct children. Children are fetched only on this explicit request — the
 * client never pre-fetches a subtree. The send is gated on `readyState === OPEN`
 * ({@link safeSend}) so an expand issued during the reconnect window is dropped
 * rather than thrown, and replayed by the next re-open's resync.
 *
 * @param socket - the live WebSocket connection.
 * @param nodeId - the id of the node whose children to load.
 */
export function requestExpand(socket: WebSocket, nodeId: string): void {
	safeSend(socket, JSON.stringify({ type: 'expand', nodeId }));
}

/**
 * BFS the transitive descendants of `nodeId` down `parentId` links. Shared by
 * {@link collapse} (which discards these nodes + their incident edges) and
 * {@link descendantIds} (which exposes the same set to the render layer), so the
 * two can never disagree about what a collapse removes. `nodeId` itself is never
 * included; an absent id yields the empty set.
 *
 * @param state - the current graph state.
 * @param nodeId - the root whose descendants to collect.
 * @returns the set of transitive descendant ids (excluding `nodeId`).
 */
function collectDescendants(state: GraphState, nodeId: string): Set<string> {
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
	return removed;
}

/**
 * Return the transitive descendant ids of `nodeId` — every node reachable by
 * following `parentId` links downward from it, `nodeId` itself excluded. This is
 * exactly the set {@link collapse} discards from the store; exposing it lets the
 * render layer prune a collapsed node's descendants from its `expanded` gate, so a
 * later reconnect resync never re-`expand`s an orphan id whose children the layout
 * can no longer place (the P9-6 stale-descendant leak). The input is never mutated.
 *
 * @param state - the current graph state.
 * @param nodeId - the node whose descendants to enumerate.
 * @returns a set of descendant ids (empty when `nodeId` is a leaf or absent).
 */
export function descendantIds(state: GraphState, nodeId: string): Set<string> {
	return collectDescendants(state, nodeId);
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
	const removed = collectDescendants(state, nodeId);
	const nextNodes = new Map<string, Node>();
	for (const [id, node] of state.nodes) {
		if (!removed.has(id)) nextNodes.set(id, node);
	}
	const nextEdges = new Map<string, Edge>();
	for (const [id, edge] of state.edges) {
		if (!removed.has(edge.source) && !removed.has(edge.target)) nextEdges.set(id, edge);
	}
	return { nodes: nextNodes, edges: nextEdges, agents: state.agents, metrics: state.metrics };
}
