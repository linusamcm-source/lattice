import { describe, it, expect, beforeEach, vi } from 'vitest';
import { get } from 'svelte/store';
import {
	applyEvent,
	initialState,
	parseEnvelope,
	ingest,
	connect,
	collapse,
	requestExpand,
	graphStore,
	nodes,
	edges
} from './ws';
import type { Node, Edge, EventEnvelope } from './types';

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
});

describe('requestExpand', () => {
	it('calls socket.send exactly once with the canonical expand frame', () => {
		const send = vi.fn();
		const socket = { send } as unknown as WebSocket;
		requestExpand(socket, 'file:a.rs');
		expect(send).toHaveBeenCalledTimes(1);
		expect(send).toHaveBeenCalledWith('{"type":"expand","nodeId":"file:a.rs"}');
	});
});
