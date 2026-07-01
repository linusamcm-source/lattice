import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { get } from 'svelte/store';
import {
	applyEvent,
	initialState,
	parseEnvelope,
	ingest,
	connect,
	collapse,
	descendantIds,
	requestExpand,
	graphStore,
	nodes,
	edges,
	agents,
	metrics
} from './ws';
import type {
	AgentInfo,
	Node,
	Edge,
	EventEnvelope,
	NodeStatus,
	TestOutcome,
	MetricsUpdatePayload,
	FileParseLatency
} from './types';
// P9-6 (RED) — the resilient-socket contract lives on the ws module. Imported as a
// namespace so a not-yet-exported member (`connectionStatus`) reads as `undefined`
// at runtime (the new tests fail on the missing behaviour, not a module-load crash),
// while `svelte-check` still errors on `WS.connectionStatus` — proving the store is
// absent. `ConnectionStatus` is a type-only import: erased at runtime, so it likewise
// reports a `svelte-check` "no exported member" error without breaking `vitest`.
import * as WS from './ws';
import type { ConnectionStatus } from './ws';

const fileNode: Node = {
	id: 'file:src/x.rs',
	type: 'file',
	label: 'x.rs',
	parentId: null,
	childIds: ['fn:src/x.rs:alpha'],
	status: 'unknown'
};

const fnNode: Node = {
	id: 'fn:src/x.rs:alpha',
	type: 'function',
	label: 'alpha',
	parentId: 'file:src/x.rs',
	childIds: [],
	status: 'unknown'
};

const containsEdge: Edge = {
	id: 'e:x.rs->alpha',
	source: fileNode.id,
	target: fnNode.id,
	kind: 'contains',
	hot: false
};

const snapshotEnv: EventEnvelope = {
	v: 1,
	ts: '2026-06-27T00:00:00.000Z',
	sessionId: 'sess-test',
	type: 'snapshot',
	payload: { nodes: [fileNode, fnNode], edges: [containsEdge] }
};

function upsertEnv(node: Node): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:01.000Z',
		sessionId: 'sess-test',
		type: 'node.upsert',
		payload: { node }
	};
}

function removeEnv(id: string): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:02.000Z',
		sessionId: 'sess-test',
		type: 'node.remove',
		payload: { id }
	};
}

beforeEach(() => {
	graphStore.set(initialState());
});

describe('applyEvent reducer', () => {
	it('ingests a snapshot into the nodes map (size 2, both ids present)', () => {
		const state = applyEvent(initialState(), snapshotEnv);
		expect(state.nodes.size).toBe(2);
		expect(state.nodes.has(fileNode.id)).toBe(true);
		expect(state.nodes.has(fnNode.id)).toBe(true);
		expect(state.edges.has(containsEdge.id)).toBe(true);
	});

	it('applies node.upsert updating an existing label alpha -> beta', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		const state = applyEvent(base, upsertEnv({ ...fnNode, label: 'beta' }));
		expect(state.nodes.size).toBe(2);
		expect(state.nodes.get(fnNode.id)?.label).toBe('beta');
	});

	it('applies node.remove deleting by id', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		const state = applyEvent(base, removeEnv(fnNode.id));
		expect(state.nodes.has(fnNode.id)).toBe(false);
		expect(state.nodes.size).toBe(1);
	});

	it('applies edge.upsert and edge.remove deltas', () => {
		let state = applyEvent(initialState(), snapshotEnv);
		const hotEdge: Edge = { ...containsEdge, hot: true };
		state = applyEvent(state, {
			v: 1,
			ts: '2026-06-27T00:00:03.000Z',
			sessionId: 'sess-test',
			type: 'edge.upsert',
			payload: { edge: hotEdge }
		});
		expect(state.edges.get(containsEdge.id)?.hot).toBe(true);
		state = applyEvent(state, {
			v: 1,
			ts: '2026-06-27T00:00:04.000Z',
			sessionId: 'sess-test',
			type: 'edge.remove',
			payload: { id: containsEdge.id }
		});
		expect(state.edges.has(containsEdge.id)).toBe(false);
	});

	it('is pure and does not mutate the input state', () => {
		const base = initialState();
		applyEvent(base, snapshotEnv);
		expect(base.nodes.size).toBe(0);
		expect(base.edges.size).toBe(0);
	});
});

describe('nodes store ingest', () => {
	it('populates the nodes store with length 2 and both ids', () => {
		ingest(snapshotEnv);
		const list = get(nodes);
		expect(list.length).toBe(2);
		expect(list.map((n) => n.id)).toEqual(expect.arrayContaining([fileNode.id, fnNode.id]));
	});

	it('renames a function node label via node.upsert', () => {
		ingest(snapshotEnv);
		ingest(upsertEnv({ ...fnNode, label: 'beta' }));
		expect(get(nodes).find((n) => n.id === fnNode.id)?.label).toBe('beta');
	});

	it('removes a node via node.remove', () => {
		ingest(snapshotEnv);
		ingest(removeEnv(fnNode.id));
		expect(get(nodes).some((n) => n.id === fnNode.id)).toBe(false);
		expect(get(edges).length).toBe(1);
	});
});

describe('parseEnvelope', () => {
	it('parses a valid JSON envelope string', () => {
		const env = parseEnvelope(JSON.stringify(snapshotEnv));
		expect(env?.type).toBe('snapshot');
	});

	it('parses an already-decoded object envelope', () => {
		expect(parseEnvelope(snapshotEnv)?.type).toBe('snapshot');
	});

	it('returns null for malformed JSON', () => {
		expect(parseEnvelope('{not json')).toBeNull();
	});

	it('returns null for an unknown event type', () => {
		expect(
			parseEnvelope(JSON.stringify({ v: 1, ts: '', sessionId: '', type: 'bogus', payload: {} }))
		).toBeNull();
	});

	it('returns null for non-object input', () => {
		expect(parseEnvelope(42)).toBeNull();
	});
});

describe('connect', () => {
	class MockSocket {
		url: string;
		closed = false;
		private listeners: Record<string, Array<(ev: { data: string }) => void>> = {};
		constructor(url: string) {
			this.url = url;
		}
		addEventListener(type: string, cb: (ev: { data: string }) => void): void {
			(this.listeners[type] ??= []).push(cb);
		}
		removeEventListener(): void {}
		close(): void {
			this.closed = true;
		}
		emit(type: string, ev: { data: string }): void {
			(this.listeners[type] ?? []).forEach((cb) => cb(ev));
		}
	}

	it('wires socket messages into the nodes store and closes', () => {
		vi.stubGlobal('WebSocket', MockSocket as unknown as typeof WebSocket);
		const client = connect('ws://localhost:9999');
		const sock = client.socket as unknown as MockSocket;
		sock.emit('message', { data: JSON.stringify(snapshotEnv) });
		expect(get(nodes).length).toBe(2);
		client.close();
		expect(sock.closed).toBe(true);
		vi.unstubAllGlobals();
	});

	it('ignores malformed socket messages without throwing', () => {
		vi.stubGlobal('WebSocket', MockSocket as unknown as typeof WebSocket);
		const client = connect('ws://localhost:9999');
		const sock = client.socket as unknown as MockSocket;
		sock.emit('message', { data: '{not json' });
		expect(get(nodes).length).toBe(0);
		client.close();
		vi.unstubAllGlobals();
	});
});

// --- P1-3: lazy expand / subtree / collapse fixtures ---

const aFile: Node = {
	id: 'file:a.rs',
	type: 'file',
	label: 'a.rs',
	parentId: null,
	childIds: ['fn:a.rs:alpha'],
	status: 'unknown'
};

const aFn: Node = {
	id: 'fn:a.rs:alpha',
	type: 'function',
	label: 'alpha',
	parentId: 'file:a.rs',
	childIds: ['var:a.rs:alpha:x'],
	status: 'unknown'
};

const aVar: Node = {
	id: 'var:a.rs:alpha:x',
	type: 'variable',
	label: 'x',
	parentId: 'fn:a.rs:alpha',
	childIds: [],
	status: 'unknown'
};

const aFileFnEdge: Edge = {
	id: 'e:a.rs->alpha',
	source: aFile.id,
	target: aFn.id,
	kind: 'contains',
	hot: false
};

const aFnVarEdge: Edge = {
	id: 'e:alpha->x',
	source: aFn.id,
	target: aVar.id,
	kind: 'contains',
	hot: false
};

function subtreeEnv(parentId: string, subNodes: Node[], subEdges: Edge[]): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:05.000Z',
		sessionId: 'sess-test',
		type: 'subtree',
		payload: { parentId, nodes: subNodes, edges: subEdges }
	};
}

describe('parseEnvelope (subtree)', () => {
	it('accepts a valid subtree envelope (non-null)', () => {
		const env = parseEnvelope(JSON.stringify(subtreeEnv(aFile.id, [aFn], [aFileFnEdge])));
		expect(env?.type).toBe('subtree');
	});

	it('returns null for a subtree envelope missing its payload', () => {
		expect(
			parseEnvelope(JSON.stringify({ v: 1, ts: '', sessionId: '', type: 'subtree' }))
		).toBeNull();
	});
});

describe('applyEvent (subtree merge)', () => {
	it('merges children into a store pre-loaded with the parent (both ids present)', () => {
		const pre = applyEvent(initialState(), upsertEnv(aFile));
		const state = applyEvent(pre, subtreeEnv(aFile.id, [aFn], [aFileFnEdge]));
		expect(state.nodes.has(aFile.id)).toBe(true);
		expect(state.nodes.has(aFn.id)).toBe(true);
		expect(state.edges.has(aFileFnEdge.id)).toBe(true);
	});

	it('is pure and does not mutate the input state', () => {
		const pre = applyEvent(initialState(), upsertEnv(aFile));
		applyEvent(pre, subtreeEnv(aFile.id, [aFn], [aFileFnEdge]));
		expect(pre.nodes.size).toBe(1);
		expect(pre.edges.size).toBe(0);
	});
});

describe('collapse', () => {
	function loadedTree() {
		let state = applyEvent(initialState(), upsertEnv(aFile));
		state = applyEvent(state, upsertEnv(aFn));
		state = applyEvent(state, upsertEnv(aVar));
		state = applyEvent(state, {
			v: 1,
			ts: '',
			sessionId: '',
			type: 'edge.upsert',
			payload: { edge: aFileFnEdge }
		});
		state = applyEvent(state, {
			v: 1,
			ts: '',
			sessionId: '',
			type: 'edge.upsert',
			payload: { edge: aFnVarEdge }
		});
		return state;
	}

	it('removes transitive descendants (function and variable) but keeps the node', () => {
		const collapsed = collapse(loadedTree(), 'file:a.rs');
		expect(collapsed.nodes.has('file:a.rs')).toBe(true);
		expect(collapsed.nodes.has('fn:a.rs:alpha')).toBe(false);
		expect(collapsed.nodes.has('var:a.rs:alpha:x')).toBe(false);
	});

	it('drops edges whose source or target was removed', () => {
		const collapsed = collapse(loadedTree(), 'file:a.rs');
		expect(collapsed.edges.has(aFileFnEdge.id)).toBe(false);
		expect(collapsed.edges.has(aFnVarEdge.id)).toBe(false);
	});

	it('is pure and does not mutate the input state', () => {
		const state = loadedTree();
		collapse(state, 'file:a.rs');
		expect(state.nodes.size).toBe(3);
		expect(state.edges.size).toBe(2);
	});

	// descendantIds shares its BFS with collapse, so Graph.svelte's toggle() can prune
	// exactly the ids collapse discards from the render-side `expanded` open set.
	describe('descendantIds', () => {
		function loadedTree() {
			let state = applyEvent(initialState(), upsertEnv(aFile));
			state = applyEvent(state, upsertEnv(aFn));
			state = applyEvent(state, upsertEnv(aVar));
			return state;
		}

		it('returns the transitive descendant ids (function + variable), excluding the node itself', () => {
			const ids = descendantIds(loadedTree(), 'file:a.rs');
			expect(ids.has('fn:a.rs:alpha')).toBe(true);
			expect(ids.has('var:a.rs:alpha:x')).toBe(true);
			expect(ids.has('file:a.rs')).toBe(false);
			expect(ids.size).toBe(2);
		});

		it('returns an empty set for a leaf node', () => {
			expect(descendantIds(loadedTree(), 'var:a.rs:alpha:x').size).toBe(0);
		});

		it('returns an empty set for an absent node id', () => {
			expect(descendantIds(loadedTree(), 'fn:ghost').size).toBe(0);
		});

		it('matches exactly the ids collapse discards', () => {
			const tree = loadedTree();
			const collapsed = collapse(tree, 'file:a.rs');
			for (const id of descendantIds(tree, 'file:a.rs')) {
				expect(collapsed.nodes.has(id)).toBe(false);
			}
			expect(collapsed.nodes.has('file:a.rs')).toBe(true);
		});
	});
});

describe('requestExpand', () => {
	it('calls socket.send exactly once with the canonical expand frame when the socket is open', () => {
		const send = vi.fn();
		// The P9-6 readyState guard sends only on an OPEN socket, so the fixture carries
		// `readyState: OPEN` — the frame still goes out exactly once.
		const socket = { send, readyState: WebSocket.OPEN } as unknown as WebSocket;
		requestExpand(socket, 'file:a.rs');
		expect(send).toHaveBeenCalledTimes(1);
		expect(send).toHaveBeenCalledWith('{"type":"expand","nodeId":"file:a.rs"}');
	});

	it('does not send (and does not throw) when the socket is not open', () => {
		const send = vi.fn();
		const socket = { send, readyState: WebSocket.CONNECTING } as unknown as WebSocket;
		expect(() => requestExpand(socket, 'file:a.rs')).not.toThrow();
		expect(send).not.toHaveBeenCalled();
	});
});

// --- P5-1: test.result / status.update wire + reducer fixtures ---

function testResultEnv(nodeId: string, outcome: TestOutcome): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:06.000Z',
		sessionId: 'sess-test',
		type: 'test.result',
		payload: { nodeId, testId: 'x::t1', outcome, sessionId: 'sess-test' }
	};
}

function statusUpdateEnv(nodeId: string, status: NodeStatus): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:07.000Z',
		sessionId: 'sess-test',
		type: 'status.update',
		payload: { nodeId, status, sessionId: 'sess-test' }
	};
}

describe('parseEnvelope (test.result / status.update)', () => {
	it('parses a valid test.result envelope (non-null, typed)', () => {
		const env = parseEnvelope(JSON.stringify(testResultEnv('fn:src/x.rs:alpha', 'fail')));
		expect(env?.type).toBe('test.result');
	});

	it('parses a valid status.update envelope (non-null)', () => {
		const env = parseEnvelope(JSON.stringify(statusUpdateEnv('fn:src/x.rs:alpha', 'passing')));
		expect(env?.type).toBe('status.update');
	});

	it('returns null for a test.result missing a string nodeId', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: '',
					type: 'test.result',
					payload: { testId: 't', outcome: 'fail', sessionId: '' }
				})
			)
		).toBeNull();
	});

	it('still returns null for a genuinely unknown type', () => {
		expect(
			parseEnvelope(JSON.stringify({ v: 1, ts: '', sessionId: '', type: 'bogus', payload: {} }))
		).toBeNull();
	});
});

describe('applyEvent (test.result / status.update)', () => {
	it('sets an existing node status to failing on a fail outcome', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		const state = applyEvent(base, testResultEnv(fnNode.id, 'fail'));
		expect(state.nodes.get(fnNode.id)?.status).toBe('failing');
	});

	it('sets an existing node status to passing on a pass outcome', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		const state = applyEvent(base, testResultEnv(fnNode.id, 'pass'));
		expect(state.nodes.get(fnNode.id)?.status).toBe('passing');
	});

	it('maps skip to stale and running to running', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		expect(applyEvent(base, testResultEnv(fnNode.id, 'skip')).nodes.get(fnNode.id)?.status).toBe(
			'stale'
		);
		expect(applyEvent(base, testResultEnv(fnNode.id, 'running')).nodes.get(fnNode.id)?.status).toBe(
			'running'
		);
	});

	it('applies a status.update to an existing node', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		const state = applyEvent(base, statusUpdateEnv(fnNode.id, 'running'));
		expect(state.nodes.get(fnNode.id)?.status).toBe('running');
	});

	it('is a no-op for a test.result whose node is absent (no phantom node)', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		const state = applyEvent(base, testResultEnv('fn:src/x.rs:ghost', 'fail'));
		expect(state.nodes.has('fn:src/x.rs:ghost')).toBe(false);
		expect(state.nodes.size).toBe(2);
		expect(state).toBe(base);
	});

	it('is a no-op for a status.update whose node is absent', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		const state = applyEvent(base, statusUpdateEnv('fn:src/x.rs:ghost', 'failing'));
		expect(state.nodes.has('fn:src/x.rs:ghost')).toBe(false);
		expect(state).toBe(base);
	});

	it('does not mutate the input state on a test.result', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		const before = base.nodes.get(fnNode.id)?.status;
		applyEvent(base, testResultEnv(fnNode.id, 'fail'));
		expect(base.nodes.get(fnNode.id)?.status).toBe(before);
	});
});

// --- P6-1: hot_edge wire + reducer fixtures ---

function hotEdgeEnv(edgeId: string, state: 'enter' | 'exit'): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:08.000Z',
		sessionId: 'sess-test',
		type: 'hot_edge',
		payload: { edgeId, state, sessionId: 'sess-test', ts: '2026-06-27T00:00:08.000Z' }
	};
}

describe('parseEnvelope (hot_edge)', () => {
	it('parses a well-formed hot_edge enter envelope (non-null, typed)', () => {
		const env = parseEnvelope(JSON.stringify(hotEdgeEnv(containsEdge.id, 'enter')));
		expect(env?.type).toBe('hot_edge');
	});

	it('parses a well-formed hot_edge exit envelope (non-null)', () => {
		expect(parseEnvelope(JSON.stringify(hotEdgeEnv(containsEdge.id, 'exit')))?.type).toBe(
			'hot_edge'
		);
	});

	it('returns null for a hot_edge missing a string edgeId', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: '',
					type: 'hot_edge',
					payload: { state: 'enter', sessionId: '' }
				})
			)
		).toBeNull();
	});

	it('returns null for a hot_edge whose edgeId is a non-string (AC5 non-string clause)', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: '',
					type: 'hot_edge',
					payload: { edgeId: 42, state: 'enter', sessionId: '' }
				})
			)
		).toBeNull();
	});

	it('returns null for a hot_edge whose state is neither enter nor exit', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: '',
					type: 'hot_edge',
					payload: { edgeId: containsEdge.id, state: 'blink', sessionId: '' }
				})
			)
		).toBeNull();
	});
});

describe('applyEvent (hot_edge)', () => {
	it('sets an existing edge hot=true on an enter event', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		const state = applyEvent(base, hotEdgeEnv(containsEdge.id, 'enter'));
		expect(state.edges.get(containsEdge.id)?.hot).toBe(true);
	});

	it('clears an existing edge hot=false on an exit event', () => {
		let state = applyEvent(initialState(), snapshotEnv);
		state = applyEvent(state, hotEdgeEnv(containsEdge.id, 'enter'));
		state = applyEvent(state, hotEdgeEnv(containsEdge.id, 'exit'));
		expect(state.edges.get(containsEdge.id)?.hot).toBe(false);
	});

	it('is a no-op for a hot_edge whose edge is absent (same state object)', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		const state = applyEvent(base, hotEdgeEnv('e:ghost->none', 'enter'));
		expect(state).toBe(base);
		expect(state.edges.has('e:ghost->none')).toBe(false);
	});

	it('does not mutate the input state on a hot_edge enter', () => {
		const base = applyEvent(initialState(), snapshotEnv);
		const before = base.edges.get(containsEdge.id)?.hot;
		applyEvent(base, hotEdgeEnv(containsEdge.id, 'enter'));
		expect(base.edges.get(containsEdge.id)?.hot).toBe(before);
	});
});

// --- P8-5: agent.roster / agent.activity ingest + roster state ---
//
// Field names mirror the Rust serde-camelCase wire (crates/backend/src/wire.rs:
// `Payload::AgentRoster`/`Payload::AgentActivity` + `AgentInfo`) and the
// DATA_MODEL.md §A.5 literals exactly. RED until P8-5 adds the union arms,
// `AgentInfo`/payload interfaces, `GraphState.agents`, the reducer branches, and
// the derived `agents` store.

const tddGreen: AgentInfo = {
	processId: 48213,
	agentId: 'tdd-green',
	agentType: 'implementation',
	color: '#2ecc71',
	status: 'active'
};

const securityScanner: AgentInfo = {
	processId: 48590,
	agentId: 'security-scanner',
	agentType: 'security',
	color: '#e67e22',
	status: 'inactive'
};

function agentRosterEnv(roster: AgentInfo[]): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:09.000Z',
		sessionId: 'sess-abc123',
		type: 'agent.roster',
		payload: { sessionId: 'sess-abc123', agents: roster }
	};
}

function agentActivityEnv(agentId: string, nodeId: string): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:10.000Z',
		sessionId: 'sess-abc123',
		type: 'agent.activity',
		payload: {
			agentId,
			action: 'modified',
			nodeId,
			sessionId: 'sess-abc123',
			processId: 48590
		}
	};
}

describe('parseEnvelope (agent.roster / agent.activity)', () => {
	it('parses a well-formed agent.roster envelope (non-null, typed)', () => {
		const env = parseEnvelope(JSON.stringify(agentRosterEnv([tddGreen, securityScanner])));
		expect(env?.type).toBe('agent.roster');
	});

	it('parses a well-formed agent.activity envelope (non-null, typed)', () => {
		const env = parseEnvelope(
			JSON.stringify(agentActivityEnv('security-scanner', 'fn:src/auth/token.rs:verify_token'))
		);
		expect(env?.type).toBe('agent.activity');
	});

	it('returns null for an agent.roster missing its agents array', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: 'sess-abc123',
					type: 'agent.roster',
					payload: { sessionId: 'sess-abc123' }
				})
			)
		).toBeNull();
	});

	it('returns null for an agent.activity missing a string agentId', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: 'sess-abc123',
					type: 'agent.activity',
					payload: {
						action: 'modified',
						nodeId: 'fn:src/auth/token.rs:verify_token',
						sessionId: 'sess-abc123'
					}
				})
			)
		).toBeNull();
	});

	it('never throws on a malformed agent payload (returns null)', () => {
		expect(() =>
			parseEnvelope(
				JSON.stringify({ v: 1, ts: '', sessionId: '', type: 'agent.roster', payload: 5 })
			)
		).not.toThrow();
		expect(
			parseEnvelope(
				JSON.stringify({ v: 1, ts: '', sessionId: '', type: 'agent.roster', payload: 5 })
			)
		).toBeNull();
	});
});

describe('applyEvent (agent.roster roster state)', () => {
	it('populates GraphState.agents keyed by processId (string key)', () => {
		const state = applyEvent(initialState(), agentRosterEnv([tddGreen, securityScanner]));
		expect(state.agents.size).toBe(2);
		const a = state.agents.get('48213');
		expect(a?.agentId).toBe('tdd-green');
		expect(a?.agentType).toBe('implementation');
		expect(a?.color).toBe('#2ecc71');
		expect(a?.status).toBe('active');
	});

	it('flips a process to inactive on a second roster', () => {
		const first = applyEvent(initialState(), agentRosterEnv([tddGreen]));
		const second = applyEvent(first, agentRosterEnv([{ ...tddGreen, status: 'inactive' }]));
		expect(second.agents.get('48213')?.status).toBe('inactive');
	});

	it('is pure: the input agents map is unchanged and a new Map is returned', () => {
		const first = applyEvent(initialState(), agentRosterEnv([tddGreen]));
		const second = applyEvent(first, agentRosterEnv([{ ...tddGreen, status: 'inactive' }]));
		expect(first.agents.get('48213')?.status).toBe('active');
		expect(second.agents).not.toBe(first.agents);
	});
});

describe('agents store ingest', () => {
	it('emits the current roster after ingesting an agent.roster', () => {
		ingest(agentRosterEnv([tddGreen, securityScanner]));
		const roster = get(agents);
		expect(roster.length).toBe(2);
		expect(roster.map((a) => a.agentId)).toEqual(
			expect.arrayContaining(['tdd-green', 'security-scanner'])
		);
	});
});

// Agent structure (an `agent` node + an `authored_by` edge) must ride the
// existing node.upsert / edge.upsert channels — these stay GREEN, proving the
// agent layer reuses the structural graph rather than a parallel store.

const agentNode: Node = {
	id: 'agent:security-scanner',
	type: 'agent',
	label: 'security-scanner',
	parentId: null,
	childIds: [],
	status: 'running'
};

const authoredByEdge: Edge = {
	id: 'e:security-scanner->verify_token',
	source: agentNode.id,
	target: 'fn:src/auth/token.rs:verify_token',
	kind: 'authored_by',
	hot: false
};

function edgeUpsertEnv(edge: Edge): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:11.000Z',
		sessionId: 'sess-abc123',
		type: 'edge.upsert',
		payload: { edge }
	};
}

describe('agent structure rides existing node/edge channels', () => {
	it('folds an agent node.upsert into the nodes map (type agent)', () => {
		const state = applyEvent(initialState(), upsertEnv(agentNode));
		expect(state.nodes.get(agentNode.id)?.type).toBe('agent');
	});

	it('folds an authored_by edge.upsert into the edges map', () => {
		const state = applyEvent(initialState(), edgeUpsertEnv(authoredByEdge));
		expect(state.edges.get(authoredByEdge.id)?.kind).toBe('authored_by');
	});

	it('exposes the agent node and authored_by edge through the derived stores', () => {
		ingest(upsertEnv(agentNode));
		ingest(edgeUpsertEnv(authoredByEdge));
		expect(get(nodes).some((n) => n.id === agentNode.id && n.type === 'agent')).toBe(true);
		expect(get(edges).some((e) => e.id === authoredByEdge.id && e.kind === 'authored_by')).toBe(
			true
		);
	});
});

// --- P9-5: metrics.update wire + reducer + derived-store fixtures ---
//
// Field names + values mirror the P9-3 Rust wire contract exactly
// (crates/backend/src/wire.rs `Payload::MetricsUpdate` + DATA_MODEL.md §A.5):
//   {sessionId, ts, nodeCount, edgeCount, memoryBytes, eventsPerSecMilli,
//    parseLatency: [{filePath, durationUs}]}  — all camelCase.
// RED until P9-5 adds the `metrics.update` union arm, the `MetricsUpdatePayload`/
// `FileParseLatency` interfaces, `GraphState.metrics` (seeded null by `initialState`),
// the reducer branch, and the derived `metrics` store. No `any`: the payload is a
// typed CLV `MetricsUpdatePayload`.

const loginLatency: FileParseLatency = { filePath: 'src/auth/login.rs', durationUs: 812 };

const metricsPayload: MetricsUpdatePayload = {
	sessionId: 'sess-abc123',
	ts: '2026-06-27T10:32:01.500Z',
	nodeCount: 128,
	edgeCount: 342,
	memoryBytes: 1048576,
	eventsPerSecMilli: 4200,
	parseLatency: [loginLatency]
};

function metricsUpdateEnv(payload: MetricsUpdatePayload): EventEnvelope {
	return {
		v: 1,
		ts: payload.ts,
		sessionId: payload.sessionId,
		type: 'metrics.update',
		payload
	};
}

describe('parseEnvelope (metrics.update)', () => {
	it('parses a well-formed metrics.update envelope (non-null, typed)', () => {
		const env = parseEnvelope(JSON.stringify(metricsUpdateEnv(metricsPayload)));
		expect(env?.type).toBe('metrics.update');
	});

	it('returns null for a metrics.update missing nodeCount', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: 'sess-abc123',
					type: 'metrics.update',
					payload: {
						sessionId: 'sess-abc123',
						ts: '',
						edgeCount: 342,
						memoryBytes: 1048576,
						eventsPerSecMilli: 4200,
						parseLatency: []
					}
				})
			)
		).toBeNull();
	});

	it('returns null for a metrics.update missing sessionId', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: 'sess-abc123',
					type: 'metrics.update',
					payload: {
						ts: '',
						nodeCount: 128,
						edgeCount: 342,
						memoryBytes: 1048576,
						eventsPerSecMilli: 4200,
						parseLatency: []
					}
				})
			)
		).toBeNull();
	});

	it('returns null for a metrics.update missing ts', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: 'sess-abc123',
					type: 'metrics.update',
					payload: {
						sessionId: 'sess-abc123',
						nodeCount: 128,
						edgeCount: 342,
						memoryBytes: 1048576,
						eventsPerSecMilli: 4200,
						parseLatency: []
					}
				})
			)
		).toBeNull();
	});

	it('returns null for a metrics.update missing parseLatency', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: 'sess-abc123',
					type: 'metrics.update',
					payload: {
						sessionId: 'sess-abc123',
						ts: '',
						nodeCount: 128,
						edgeCount: 342,
						memoryBytes: 1048576,
						eventsPerSecMilli: 4200
					}
				})
			)
		).toBeNull();
	});

	it('returns null when parseLatency is not an array', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: 'sess-abc123',
					type: 'metrics.update',
					payload: {
						sessionId: 'sess-abc123',
						ts: '',
						nodeCount: 128,
						edgeCount: 342,
						memoryBytes: 1048576,
						eventsPerSecMilli: 4200,
						parseLatency: 'not-an-array'
					}
				})
			)
		).toBeNull();
	});

	it('returns null when a parseLatency row is not { filePath, durationUs }', () => {
		expect(
			parseEnvelope(
				JSON.stringify({
					v: 1,
					ts: '',
					sessionId: 'sess-abc123',
					type: 'metrics.update',
					payload: {
						sessionId: 'sess-abc123',
						ts: '',
						nodeCount: 128,
						edgeCount: 342,
						memoryBytes: 1048576,
						eventsPerSecMilli: 4200,
						parseLatency: [{ filePath: 'src/auth/login.rs' }]
					}
				})
			)
		).toBeNull();
	});

	it('never throws on a malformed metrics payload (returns null)', () => {
		expect(() =>
			parseEnvelope(
				JSON.stringify({ v: 1, ts: '', sessionId: '', type: 'metrics.update', payload: 7 })
			)
		).not.toThrow();
		expect(
			parseEnvelope(
				JSON.stringify({ v: 1, ts: '', sessionId: '', type: 'metrics.update', payload: 7 })
			)
		).toBeNull();
	});
});

describe('applyEvent (metrics.update)', () => {
	it('stores the metrics payload on GraphState.metrics', () => {
		const state = applyEvent(initialState(), metricsUpdateEnv(metricsPayload));
		expect(state.metrics).toEqual(metricsPayload);
	});

	it('replaces the stored metrics on a later metrics.update', () => {
		const first = applyEvent(initialState(), metricsUpdateEnv(metricsPayload));
		const next: MetricsUpdatePayload = { ...metricsPayload, nodeCount: 200, edgeCount: 500 };
		const second = applyEvent(first, metricsUpdateEnv(next));
		expect(second.metrics?.nodeCount).toBe(200);
		expect(second.metrics?.edgeCount).toBe(500);
	});

	it('seeds GraphState.metrics as null in initialState', () => {
		expect(initialState().metrics).toBeNull();
	});

	it('is pure: the input state is unchanged and a fresh state is returned', () => {
		const base = initialState();
		const state = applyEvent(base, metricsUpdateEnv(metricsPayload));
		expect(base.metrics).toBeNull();
		expect(state).not.toBe(base);
	});
});

describe('metrics store ingest', () => {
	it('emits null before any metrics.update', () => {
		expect(get(metrics)).toBeNull();
	});

	it('emits the current metrics after ingesting a metrics.update', () => {
		ingest(metricsUpdateEnv(metricsPayload));
		expect(get(metrics)?.nodeCount).toBe(128);
		expect(get(metrics)?.parseLatency[0]?.filePath).toBe('src/auth/login.rs');
	});
});

// --- P9-6: WS reconnect + backoff + resync + bounded-memory regression (RED) ---
//
// Today `connect(url)` (ws.ts:355-363) registers ONLY a 'message' listener and the
// returned `WsClient` is just `{ socket, close }` — a dropped socket dies silently and
// callers (`+page.svelte`, `Graph.svelte`) hold a one-shot `socket`, so a naive
// reconnect would write to a DEAD socket. These tests pin the resilient contract GREEN
// must implement. They are RED until ws.ts adds it (svelte-check errors on the missing
// `connectionStatus` store / `ConnectionStatus` type / `connect` options arg / the
// `WsClient.requestExpand`/`send` handle methods; vitest fails because no reconnect,
// backoff, resync-send, or status transition exists yet).
//
// THE CONTRACT GREEN MUST ADD (referenced below):
//   export type ConnectionStatus = 'connecting' | 'open' | 'reconnecting' | 'closed';
//   export const connectionStatus: Writable<ConnectionStatus>;   // drives the UI badge
//   export interface WsClient {
//     socket: WebSocket;                       // the CURRENT live socket (swapped on reconnect)
//     requestExpand: (nodeId: string) => void; // ALWAYS targets the live socket
//     send: (data: string) => void;            // ALWAYS targets the live socket
//     close: () => void;                        // intentional teardown; stops reconnecting
//   }
//   export interface ConnectOptions {
//     // How the open-node set (Graph.svelte-local `expanded`) crosses the layer:
//     getExpandedNodes?: () => Iterable<string>;
//     backoff?: { baseMs: number; maxMs: number; jitter?: () => number };
//   }
//   export function connect(url: string, options?: ConnectOptions): WsClient;
//
// Lifecycle GREEN must implement:
//   - register 'open' / 'close' / 'error' listeners (not just 'message');
//   - status: 'connecting' on connect → 'open' on 'open' → 'reconnecting' on drop →
//     'open' on recovery → 'closed' on intentional `close()`;
//   - on 'close'/'error' schedule a reconnect via setTimeout with delay
//     `min(maxMs, baseMs * 2 ** (attempt - 1)) + jitter()`; reset `attempt` on a
//     successful re-open;
//   - on every re-open send `{"type":"snapshot"}` FIRST, then one
//     `requestExpand(id)` per id from `getExpandedNodes()`.

// The extended MockSocket the AC calls for (ws.test.ts:186-203 today has only
// `emit`/`close` and no recorder/registry): adds a `sends: string[]` recorder, a
// `readyState` + `OPEN` constant, and a STATIC registry of constructed instances so a
// test can grab the post-reconnect socket. `close()` does NOT fire a 'close' event
// (mirrors the real teardown-vs-drop split) — a drop is simulated with
// `sock.emit('close')`. `emit('open')` flips `readyState` to OPEN *before* firing
// listeners, so a GREEN handler that gates its resync-send on `readyState === OPEN`
// still fires.
class ReconnectMockSocket {
	static readonly CONNECTING = 0;
	static readonly OPEN = 1;
	static readonly CLOSING = 2;
	static readonly CLOSED = 3;
	/** Every socket the client constructed, in order (index 0 = first connect). */
	static instances: ReconnectMockSocket[] = [];
	static reset(): void {
		ReconnectMockSocket.instances = [];
	}

	readonly CONNECTING = 0;
	readonly OPEN = 1;
	readonly CLOSING = 2;
	readonly CLOSED = 3;

	url: string;
	readyState = ReconnectMockSocket.CONNECTING;
	closed = false;
	/** Every frame passed to `send`, in order — the resync/re-expand assertion target. */
	sends: string[] = [];
	private listeners: Record<string, Array<(ev: unknown) => void>> = {};

	constructor(url: string) {
		this.url = url;
		ReconnectMockSocket.instances.push(this);
	}

	addEventListener(type: string, cb: (ev: unknown) => void): void {
		(this.listeners[type] ??= []).push(cb);
	}
	removeEventListener(type: string, cb: (ev: unknown) => void): void {
		this.listeners[type] = (this.listeners[type] ?? []).filter((l) => l !== cb);
	}
	send(data: string): void {
		this.sends.push(data);
	}
	close(): void {
		this.closed = true;
		this.readyState = ReconnectMockSocket.CLOSED;
	}
	emit(type: string, ev: unknown = {}): void {
		if (type === 'open') this.readyState = ReconnectMockSocket.OPEN;
		if (type === 'close' || type === 'error') this.readyState = ReconnectMockSocket.CLOSED;
		(this.listeners[type] ?? []).forEach((cb) => cb(ev));
	}
}

/** Deterministic backoff for the schedule assertions: jitter zeroed so delays are exact. */
const BACKOFF = { baseMs: 100, maxMs: 1000, jitter: () => 0 };

/** Install the extended mock as the global WebSocket and clear its registry. */
function installReconnectMock(): void {
	ReconnectMockSocket.reset();
	vi.stubGlobal('WebSocket', ReconnectMockSocket as unknown as typeof WebSocket);
}

/** Open a client through the new resilient `connect(url, options)` surface. */
function connectResilient(expanded: Set<string> = new Set<string>()) {
	// Second `connect` arg + the options shape are the P9-6 contract → svelte-check RED.
	return connect('ws://localhost:9999', {
		getExpandedNodes: () => expanded,
		backoff: BACKOFF
	});
}

/** Normalise a graph state to sorted, comparable entries (Maps aren't `toEqual`-friendly). */
function stateSig(s: {
	nodes: Map<string, Node>;
	edges: Map<string, Edge>;
	agents: Map<string, AgentInfo>;
}) {
	const byKey = <V>(m: Map<string, V>): Array<[string, V]> =>
		[...m.entries()].sort(([a], [b]) => a.localeCompare(b));
	return { nodes: byKey(s.nodes), edges: byKey(s.edges), agents: byKey(s.agents) };
}

// A one-file root subtree the mock-server replays on connect and again on resync.
const aRootSnapshot: EventEnvelope = {
	v: 1,
	ts: '2026-06-30T00:00:00.000Z',
	sessionId: 'sess-test',
	type: 'snapshot',
	payload: { nodes: [aFile], edges: [] }
};

/** The server's connect/resync reply: root snapshot, then the P9-7 roster trailer, then
 *  the re-requested subtree — exactly what a reconnect must fold back to pre-drop state. */
function serverReplay(sock: ReconnectMockSocket): void {
	sock.emit('message', { data: JSON.stringify(aRootSnapshot) });
	sock.emit('message', { data: JSON.stringify(agentRosterEnv([tddGreen, securityScanner])) });
	sock.emit('message', { data: JSON.stringify(subtreeEnv('file:a.rs', [aFn], [aFileFnEdge])) });
}

const SNAPSHOT_FRAME = '{"type":"snapshot"}';
const expandFrame = (nodeId: string): string => JSON.stringify({ type: 'expand', nodeId });

describe('P9-6 reconnect backoff schedule', () => {
	beforeEach(() => {
		vi.useFakeTimers();
		installReconnectMock();
	});
	afterEach(() => {
		vi.useRealTimers();
		vi.unstubAllGlobals();
	});

	it('a drop schedules a reconnect that only fires after the base backoff delay', () => {
		const inst = ReconnectMockSocket.instances;
		connectResilient();
		inst[0].emit('open');
		inst[0].emit('close'); // socket dropped → schedule attempt #1 (base = 100ms)

		expect(inst.length).toBe(1);
		vi.advanceTimersByTime(99);
		expect(inst.length).toBe(1); // not yet — delay not elapsed
		vi.advanceTimersByTime(1);
		expect(inst.length).toBe(2); // reconnect socket constructed at exactly 100ms
	});

	it('consecutive failures grow the backoff exponentially up to the cap', () => {
		const inst = ReconnectMockSocket.instances;
		connectResilient();
		inst[0].emit('open');

		inst[0].emit('close'); // attempt #1 → 100ms
		vi.advanceTimersByTime(100);
		expect(inst.length).toBe(2);

		inst[1].emit('close'); // attempt #2 → 200ms (base * 2)
		vi.advanceTimersByTime(199);
		expect(inst.length).toBe(2);
		vi.advanceTimersByTime(1);
		expect(inst.length).toBe(3);

		inst[2].emit('close'); // attempt #3 → 400ms (base * 4)
		vi.advanceTimersByTime(399);
		expect(inst.length).toBe(3);
		vi.advanceTimersByTime(1);
		expect(inst.length).toBe(4);

		inst[3].emit('close'); // attempt #4 → 800ms (base * 8)
		vi.advanceTimersByTime(800);
		expect(inst.length).toBe(5);

		inst[4].emit('close'); // attempt #5 → 1600ms capped to maxMs = 1000ms
		vi.advanceTimersByTime(999);
		expect(inst.length).toBe(5);
		vi.advanceTimersByTime(1);
		expect(inst.length).toBe(6); // fired at the cap, not at 1600ms
	});

	it('a successful re-open resets the backoff to the base delay', () => {
		const inst = ReconnectMockSocket.instances;
		connectResilient();
		inst[0].emit('open');

		inst[0].emit('close'); // #1 → 100ms
		vi.advanceTimersByTime(100);
		inst[1].emit('close'); // #2 → 200ms (backoff has grown)
		vi.advanceTimersByTime(200);
		expect(inst.length).toBe(3);

		inst[2].emit('open'); // recovery → backoff resets
		inst[2].emit('close'); // next attempt must be the BASE delay again (100ms), not 400ms
		vi.advanceTimersByTime(99);
		expect(inst.length).toBe(3);
		vi.advanceTimersByTime(1);
		expect(inst.length).toBe(4);
	});
});

describe('P9-6 resync + re-expand on re-open', () => {
	beforeEach(() => {
		vi.useFakeTimers();
		installReconnectMock();
	});
	afterEach(() => {
		vi.useRealTimers();
		vi.unstubAllGlobals();
	});

	it('on re-open sends {"type":"snapshot"} first then an expand per open node, and folds back to the pre-drop state', () => {
		const inst = ReconnectMockSocket.instances;
		const expanded = new Set<string>(['file:a.rs']);
		connectResilient(expanded);

		// Initial connect: server replays the graph + roster + subtree → capture pre-drop state.
		inst[0].emit('open');
		serverReplay(inst[0]);
		const pre = stateSig(get(graphStore));
		expect(pre.nodes.length).toBe(2); // aFile + aFn
		expect(pre.agents.length).toBe(2);

		// Drop → backoff → the reconnect socket comes up.
		inst[0].emit('close');
		vi.advanceTimersByTime(BACKOFF.baseMs);
		const reconnect = inst[1];
		reconnect.emit('open');

		// The resync frame goes out first, then one expand per still-open node — asserted
		// on the POST-RECONNECT socket's recorder.
		expect(reconnect.sends[0]).toBe(SNAPSHOT_FRAME);
		expect(reconnect.sends).toContain(expandFrame('file:a.rs'));

		// Server replays the same graph on the new socket → state is identical to pre-drop
		// (BUILD_PLAN cross-cutting: "a graph identical to the server's state"), roster incl.
		serverReplay(reconnect);
		expect(stateSig(get(graphStore))).toEqual(pre);
	});
});

describe('P9-6 stable handle targets the live socket (no stale socket)', () => {
	beforeEach(() => {
		vi.useFakeTimers();
		installReconnectMock();
	});
	afterEach(() => {
		vi.useRealTimers();
		vi.unstubAllGlobals();
	});

	it('a user-initiated expand after reconnect goes to the NEW socket, never the dead one', () => {
		const inst = ReconnectMockSocket.instances;
		const client = connectResilient();
		inst[0].emit('open');

		// Drop → reconnect → the new live socket.
		inst[0].emit('close');
		vi.advanceTimersByTime(BACKOFF.baseMs);
		const reconnect = inst[1];
		reconnect.emit('open');

		// The handle's requestExpand/send must resolve the CURRENT socket, not the captured one.
		client.requestExpand('file:zzz');
		client.send('ping');

		expect(reconnect.sends).toContain(expandFrame('file:zzz'));
		expect(reconnect.sends).toContain('ping');
		// The dropped socket never received the post-reconnect frames.
		expect(inst[0].sends).not.toContain(expandFrame('file:zzz'));
		expect(inst[0].sends).not.toContain('ping');
	});
});

describe('P9-6 send guard during the reconnect window', () => {
	beforeEach(() => {
		vi.useFakeTimers();
		installReconnectMock();
	});
	afterEach(() => {
		vi.useRealTimers();
		vi.unstubAllGlobals();
	});

	it('a requestExpand issued while the socket is down does not throw and is replayed on re-open', () => {
		const inst = ReconnectMockSocket.instances;
		const expanded = new Set<string>();
		const client = connect('ws://localhost:9999', {
			getExpandedNodes: () => expanded,
			backoff: BACKOFF
		});
		inst[0].emit('open');

		// Socket drops → the current socket is CLOSED and the replacement is not yet
		// constructed: this is the reconnect window.
		inst[0].emit('close');
		// The user clicks expand mid-reconnect. Mirror the route: the id is added to the
		// open set before the send, so resync will replay it. The guarded send must NOT
		// throw (a CONNECTING socket would raise InvalidStateError) and must NOT write to
		// the dead socket.
		expanded.add('file:mid');
		expect(() => client.requestExpand('file:mid')).not.toThrow();
		expect(inst[0].sends).not.toContain(expandFrame('file:mid'));

		// Reconnect completes → the on-open resync replays snapshot + the still-open node.
		vi.advanceTimersByTime(BACKOFF.baseMs);
		const reconnect = inst[1];
		reconnect.emit('open');
		expect(reconnect.sends[0]).toBe(SNAPSHOT_FRAME);
		expect(reconnect.sends).toContain(expandFrame('file:mid'));
	});
});

describe('P9-6 connection-status store transitions', () => {
	beforeEach(() => {
		vi.useFakeTimers();
		installReconnectMock();
	});
	afterEach(() => {
		vi.useRealTimers();
		vi.unstubAllGlobals();
	});

	it('transitions connecting → open → reconnecting → open, then closed on teardown', () => {
		const inst = ReconnectMockSocket.instances;
		const seen: ConnectionStatus[] = [];
		const client = connectResilient();
		seen.push(get(WS.connectionStatus)); // 'connecting' immediately after connect

		inst[0].emit('open');
		seen.push(get(WS.connectionStatus)); // 'open'

		inst[0].emit('close');
		seen.push(get(WS.connectionStatus)); // 'reconnecting' while the socket is down

		vi.advanceTimersByTime(BACKOFF.baseMs);
		inst[1].emit('open');
		seen.push(get(WS.connectionStatus)); // back to 'open' on recovery

		client.close();
		seen.push(get(WS.connectionStatus)); // 'closed' on intentional teardown

		expect(seen).toEqual(['connecting', 'open', 'reconnecting', 'open', 'closed']);
	});
});

describe('P9-6 bounded-memory regression', () => {
	it('expand(file) → expand(fn) → collapse(file) returns nodes/edges to baseline, no orphans', () => {
		// Regression lock on the EXISTING pure `collapse` (ws.ts:394-415) — GREEN's
		// reconnect changes must not regress the collapse-discard memory bound.
		const baseline = applyEvent(initialState(), upsertEnv(aFile));
		expect(baseline.nodes.size).toBe(1);
		expect(baseline.edges.size).toBe(0);

		let s = applyEvent(baseline, subtreeEnv('file:a.rs', [aFn], [aFileFnEdge])); // expand(file)
		s = applyEvent(s, subtreeEnv('fn:a.rs:alpha', [aVar], [aFnVarEdge])); // expand(fn)
		expect(s.nodes.size).toBe(3);
		expect(s.edges.size).toBe(2);

		const collapsed = collapse(s, 'file:a.rs');
		expect(collapsed.nodes.size).toBe(baseline.nodes.size);
		expect(collapsed.edges.size).toBe(baseline.edges.size);
		expect(collapsed.nodes.has('fn:a.rs:alpha')).toBe(false);
		expect(collapsed.nodes.has('var:a.rs:alpha:x')).toBe(false);
	});

	it('repeating the expand/collapse cycle N times never grows the store beyond baseline', () => {
		let s = applyEvent(initialState(), upsertEnv(aFile));
		const baseNodes = s.nodes.size;
		const baseEdges = s.edges.size;

		for (let i = 0; i < 8; i++) {
			s = applyEvent(s, subtreeEnv('file:a.rs', [aFn], [aFileFnEdge]));
			s = applyEvent(s, subtreeEnv('fn:a.rs:alpha', [aVar], [aFnVarEdge]));
			s = collapse(s, 'file:a.rs');
			expect(s.nodes.size).toBe(baseNodes);
			expect(s.edges.size).toBe(baseEdges);
		}
	});
});

describe('P9-6 reconnect keeps the open set consistent (no stale expanded id)', () => {
	beforeEach(() => {
		vi.useFakeTimers();
		installReconnectMock();
	});
	afterEach(() => {
		vi.useRealTimers();
		vi.unstubAllGlobals();
	});

	// De-masked: the original test hand-deleted BOTH 'file:a.rs' and 'fn:a.rs:alpha',
	// which hid that Graph.svelte's toggle() only pruned the collapsed id. This version
	// collapses through the SAME shared `descendantIds` helper toggle() now uses —
	// deleting the collapsed id plus every descendant the helper reports from the live
	// store — so a nested collapse leaves no stale descendant for reconnect to re-expand.
	// The companion Graph.test.ts test drives the real toggle() UI end to end.
	it('a nested collapse leaves no stale descendant for reconnect to re-expand', () => {
		const inst = ReconnectMockSocket.instances;
		// The render-side open set (Graph.svelte `expanded`): file expanded, then fn.
		const expanded = new Set<string>(['file:a.rs', 'fn:a.rs:alpha']);
		// The store as it stands with the whole file→fn→var tree loaded (what
		// descendantIds walks at collapse time).
		let state = applyEvent(initialState(), upsertEnv(aFile));
		state = applyEvent(state, upsertEnv(aFn));
		state = applyEvent(state, upsertEnv(aVar));
		graphStore.set(state);
		connect('ws://localhost:9999', { getExpandedNodes: () => expanded, backoff: BACKOFF });

		// First drop → reconnect re-expands every currently-open node.
		inst[0].emit('open');
		inst[0].emit('close');
		vi.advanceTimersByTime(BACKOFF.baseMs);
		inst[1].emit('open');
		expect(inst[1].sends).toContain(expandFrame('file:a.rs'));
		expect(inst[1].sends).toContain(expandFrame('fn:a.rs:alpha'));

		// User collapses `file:a.rs`. Mirror toggle() EXACTLY: delete the collapsed id
		// AND every transitive descendant the shared helper reports — not a hand-listed
		// pair — then discard the subtree from the store.
		for (const id of descendantIds(get(graphStore), 'file:a.rs')) expanded.delete(id);
		expanded.delete('file:a.rs');
		graphStore.update((s) => collapse(s, 'file:a.rs'));

		// The pruned open set carries no id absent from the (collapsed) store.
		expect(expanded.size).toBe(0);
		for (const id of expanded) expect(get(graphStore).nodes.has(id)).toBe(true);

		// Second drop → reconnect must re-expand NEITHER the collapsed id NOR its orphaned
		// descendant: only the resync frame goes out.
		inst[1].emit('close');
		vi.advanceTimersByTime(BACKOFF.baseMs);
		inst[2].emit('open');
		expect(inst[2].sends).toEqual([SNAPSHOT_FRAME]);
	});
});
