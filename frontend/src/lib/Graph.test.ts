import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, cleanup, fireEvent } from '@testing-library/svelte';
import { get } from 'svelte/store';
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
	childIds: ['var:src/x.rs:alpha:y'],
	status: 'unknown'
};

const varNode: Node = {
	id: 'var:src/x.rs:alpha:y',
	type: 'variable',
	label: 'y',
	parentId: 'fn:src/x.rs:alpha',
	childIds: [],
	status: 'unknown'
};

function snapshot(nodes: Node[]): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:00.000Z',
		sessionId: 'sess-test',
		type: 'snapshot',
		payload: { nodes, edges: [] }
	};
}

function subtree(parentId: string, nodes: Node[]): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:01.000Z',
		sessionId: 'sess-test',
		type: 'subtree',
		payload: { parentId, nodes, edges: [] }
	};
}

beforeEach(() => {
	graphStore.set(initialState());
});

afterEach(() => {
	cleanup();
});

// SvelteFlow leaves each node `visibility: hidden` until it is measured, and the
// jsdom ResizeObserver mock never fires that measurement. Hidden elements resolve
// to an empty accessible name, so role+name lookups can't see the toggle buttons;
// the buttons are still present and clickable, so we locate them by `data-testid`
// (which ignores visibility) and assert labels via text queries.
const toggleId = (nodeId: string): string => `toggle-${nodeId}`;

describe('Graph.svelte lazy hierarchy render', () => {
	it('renders only the top-level file node on mount (no function/variable)', async () => {
		ingest(snapshot([fileNode]));
		const screen = render(Graph);
		await tick();
		expect(await screen.findByText('x.rs')).toBeTruthy();
		expect(screen.queryByText('alpha')).toBeNull();
		expect(screen.queryByText('y')).toBeNull();
	});

	it('expanding a file invokes onExpand with its id, then renders the merged child', async () => {
		ingest(snapshot([fileNode]));
		const onExpand = vi.fn();
		const screen = render(Graph, { props: { onExpand } });
		await tick();

		const expandBtn = await screen.findByTestId(toggleId('file:src/x.rs'));
		expect(expandBtn.getAttribute('aria-label')).toBe('Expand x.rs');
		await fireEvent.click(expandBtn);
		expect(onExpand).toHaveBeenCalledTimes(1);
		expect(onExpand).toHaveBeenCalledWith('file:src/x.rs');

		// Child is not on the canvas until the subtree reply merges it into the store.
		expect(screen.queryByText('alpha')).toBeNull();
		ingest(subtree('file:src/x.rs', [fnNode]));
		expect(await screen.findByText('alpha')).toBeTruthy();
	});

	it('without an injected onExpand, expanding requests the subtree over the socket', async () => {
		ingest(snapshot([fileNode]));
		const send = vi.fn();
		const socket = { send } as unknown as WebSocket;
		const screen = render(Graph, { props: { socket } });
		await tick();

		await fireEvent.click(await screen.findByTestId(toggleId('file:src/x.rs')));
		expect(send).toHaveBeenCalledTimes(1);
		expect(send).toHaveBeenCalledWith('{"type":"expand","nodeId":"file:src/x.rs"}');
	});

	it('zoom-gates variables: a function child renders, but its variable only after expanding the function', async () => {
		ingest(snapshot([fileNode, fnNode, varNode]));
		const onExpand = vi.fn();
		const screen = render(Graph, { props: { onExpand } });
		await tick();

		// Nothing expanded → only the file renders.
		expect(await screen.findByText('x.rs')).toBeTruthy();
		expect(screen.queryByText('alpha')).toBeNull();
		expect(screen.queryByText('y')).toBeNull();

		// Expand the file → the function renders, but its variable is still gated.
		await fireEvent.click(await screen.findByTestId(toggleId('file:src/x.rs')));
		expect(await screen.findByText('alpha')).toBeTruthy();
		expect(screen.queryByText('y')).toBeNull();

		// Expand the function → its variable child now renders.
		await fireEvent.click(await screen.findByTestId(toggleId('fn:src/x.rs:alpha')));
		expect(await screen.findByText('y')).toBeTruthy();
	});

	it('collapsing a file discards its descendants from the canvas and the store', async () => {
		ingest(snapshot([fileNode, fnNode, varNode]));
		const onExpand = vi.fn();
		const screen = render(Graph, { props: { onExpand } });
		await tick();

		await fireEvent.click(await screen.findByTestId(toggleId('file:src/x.rs')));
		expect(await screen.findByText('alpha')).toBeTruthy();

		// The same affordance now collapses (its label has flipped).
		const collapseBtn = await screen.findByTestId(toggleId('file:src/x.rs'));
		expect(collapseBtn.getAttribute('aria-label')).toBe('Collapse x.rs');
		await fireEvent.click(collapseBtn);
		await tick();

		expect(screen.queryByText('alpha')).toBeNull();
		expect(screen.queryByText('y')).toBeNull();
		expect(await screen.findByText('x.rs')).toBeTruthy();

		const state = get(graphStore);
		expect(state.nodes.has('file:src/x.rs')).toBe(true);
		expect(state.nodes.has('fn:src/x.rs:alpha')).toBe(false);
		expect(state.nodes.has('var:src/x.rs:alpha:y')).toBe(false);
	});
});
