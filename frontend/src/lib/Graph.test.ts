import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { render, cleanup } from '@testing-library/svelte';
import { tick } from 'svelte';
import Graph from './Graph.svelte';
import { ingest, graphStore, initialState } from './ws';
import type { Node, EventEnvelope } from './types';

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

const snapshotEnv: EventEnvelope = {
	v: 1,
	ts: '2026-06-27T00:00:00.000Z',
	sessionId: 'sess-test',
	type: 'snapshot',
	payload: { nodes: [fileNode, fnNode], edges: [] }
};

const renameEnv: EventEnvelope = {
	v: 1,
	ts: '2026-06-27T00:00:01.000Z',
	sessionId: 'sess-test',
	type: 'node.upsert',
	payload: { node: { ...fnNode, label: 'beta' } }
};

beforeEach(() => {
	graphStore.set(initialState());
});

afterEach(() => {
	cleanup();
});

describe('Graph.svelte two-tier render', () => {
	it('renders the file label and its function child label', async () => {
		ingest(snapshotEnv);
		const screen = render(Graph);
		await tick();
		expect(await screen.findByText('x.rs')).toBeTruthy();
		expect(await screen.findByText('alpha')).toBeTruthy();
	});

	it('updates the rendered label when a function is renamed alpha -> beta', async () => {
		ingest(snapshotEnv);
		const screen = render(Graph);
		await tick();
		expect(await screen.findByText('alpha')).toBeTruthy();

		ingest(renameEnv);
		await tick();
		expect(await screen.findByText('beta')).toBeTruthy();
		expect(screen.queryByText('alpha')).toBeNull();
	});
});
