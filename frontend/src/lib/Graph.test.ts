import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, cleanup, fireEvent, waitFor } from '@testing-library/svelte';
import { get } from 'svelte/store';
import { tick } from 'svelte';
import Graph from './Graph.svelte';
import HierarchyNodeHarness from './HierarchyNode.harness.svelte';
import GraphExpandedHarness from './Graph.expanded.harness.svelte';
import { ingest, graphStore, initialState, requestExpand } from './ws';
// P9-6 (RED): namespace import so the not-yet-exported `connectionStatus` store reads
// as `undefined` at runtime (fails the test on the missing store, not a module crash)
// while `svelte-check` errors on `WS.connectionStatus` — proving the store is absent.
import * as WS from './ws';
import type { Node, Edge, EventEnvelope, NodeStatus, TestOutcome } from './types';

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

function upsert(node: Node): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-27T00:00:02.000Z',
		sessionId: 'sess-test',
		type: 'node.upsert',
		payload: { node }
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

	it('expanding routes the canonical expand frame through the onExpand handler', async () => {
		ingest(snapshot([fileNode]));
		const send = vi.fn();
		// Mirror +page.svelte: onExpand routes through the stable WsClient/requestExpand,
		// never a socket prop captured by Graph (the removed P9-6 foot-gun).
		const socket = { send, readyState: WebSocket.OPEN } as unknown as WebSocket;
		const onExpand = (nodeId: string): void => requestExpand(socket, nodeId);
		const screen = render(Graph, { props: { onExpand } });
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

// P3-3: doc tooltip + selection sidebar.
const selectId = (nodeId: string): string => `select-${nodeId}`;

const docFile: Node = {
	id: 'file:src/x.rs',
	type: 'file',
	label: 'x.rs',
	parentId: null,
	childIds: ['fn:src/x.rs:alpha'],
	status: 'unknown',
	docs: 'File level docs.'
};

describe('HierarchyNode doc tooltip', () => {
	it('renders data.docs as a title attribute, queryable under SvelteFlow visibility:hidden', async () => {
		const screen = render(HierarchyNodeHarness, {
			props: {
				id: 'fn:src/x.rs:alpha',
				data: {
					label: 'alpha',
					expandable: false,
					expanded: false,
					status: 'unknown',
					docs: 'Hello docs',
					onSelect: () => {},
					onToggle: () => {}
				}
			}
		});
		expect(await screen.findByTitle('Hello docs')).toBeTruthy();
	});
});

describe('Graph.svelte selection sidebar', () => {
	it('shows an empty hint until a node is selected', async () => {
		ingest(snapshot([docFile]));
		const screen = render(Graph);
		await tick();
		expect(screen.getByText(/no node selected/i)).toBeTruthy();
	});

	it('clicking a node select region shows that node docs in the sidebar', async () => {
		ingest(snapshot([docFile]));
		const screen = render(Graph);
		await tick();

		await fireEvent.click(await screen.findByTestId(selectId('file:src/x.rs')));
		expect(await screen.findByText('File level docs.')).toBeTruthy();
	});

	it('clicking the expand button does NOT select the node', async () => {
		ingest(snapshot([docFile]));
		const screen = render(Graph);
		await tick();

		// Toggling expansion must not flip selection: the sidebar stays empty.
		await fireEvent.click(await screen.findByTestId(toggleId('file:src/x.rs')));
		await tick();
		expect(screen.getByText(/no node selected/i)).toBeTruthy();
	});

	it('a node.upsert with updated docs for the selected node updates the sidebar text', async () => {
		ingest(snapshot([{ ...docFile, docs: 'v1 docs' }]));
		const screen = render(Graph);
		await tick();

		await fireEvent.click(await screen.findByTestId(selectId('file:src/x.rs')));
		expect(await screen.findByText('v1 docs')).toBeTruthy();

		ingest(upsert({ ...docFile, docs: 'v2 docs' }));
		expect(await screen.findByText('v2 docs')).toBeTruthy();
		expect(screen.queryByText('v1 docs')).toBeNull();
	});
});

// P4-4: edge rendering + control/data-flow filter.
const fnA: Node = {
	id: 'fn:src/x.rs:a',
	type: 'function',
	label: 'a',
	parentId: null,
	childIds: [],
	status: 'unknown'
};

const fnB: Node = {
	id: 'fn:src/x.rs:b',
	type: 'function',
	label: 'b',
	parentId: null,
	childIds: [],
	status: 'unknown'
};

const callsEdge: Edge = {
	id: 'e:fn:src/x.rs:b->fn:src/x.rs:a:calls',
	source: 'fn:src/x.rs:b',
	target: 'fn:src/x.rs:a',
	kind: 'calls',
	hot: false
};

function snapshotWith(nodes: Node[], edges: Edge[]): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-28T00:00:00.000Z',
		sessionId: 'sess-test',
		type: 'snapshot',
		payload: { nodes, edges }
	};
}

describe('Graph.svelte edge rendering + flow filter', () => {
	it('renders Control flow and Data flow toggles, both on by default', async () => {
		ingest(snapshotWith([fnA, fnB], [callsEdge]));
		const screen = render(Graph);
		await tick();

		const control = screen.getByRole('checkbox', { name: /control flow/i }) as HTMLInputElement;
		const data = screen.getByRole('checkbox', { name: /data flow/i }) as HTMLInputElement;
		expect(control.checked).toBe(true);
		expect(data.checked).toBe(true);
	});

	it('renders a calls edge between two visible nodes and drops it when Control flow is off', async () => {
		ingest(snapshotWith([fnA, fnB], [callsEdge]));
		const { container, getByRole } = render(Graph);
		await tick();

		// Both function nodes are roots, so both are on the canvas; the calls edge
		// between them renders once the nodes are laid out.
		await waitFor(() => expect(container.querySelectorAll('.svelte-flow__edge').length).toBe(1));

		await fireEvent.click(getByRole('checkbox', { name: /control flow/i }));
		await waitFor(() => expect(container.querySelectorAll('.svelte-flow__edge').length).toBe(0));

		// Data flow stays on, so a data-flow edge would still render.
		expect((getByRole('checkbox', { name: /data flow/i }) as HTMLInputElement).checked).toBe(true);
	});
});

// P5-5: colour nodes by status. The node carries a `data-status` marker plus a
// status-keyed colour class; SvelteFlow leaves nodes `visibility: hidden` in
// jsdom, but attributes/classes are still inspectable via a container query.
function mountStatus(status: NodeStatus) {
	return render(HierarchyNodeHarness, {
		props: {
			id: 'fn:src/x.rs:alpha',
			data: {
				label: 'alpha',
				expandable: false,
				expanded: false,
				status,
				onSelect: () => {},
				onToggle: () => {}
			}
		}
	});
}

const statusEl = (container: HTMLElement, status: NodeStatus): Element | null =>
	container.querySelector(`[data-status="${status}"]`);

describe('HierarchyNode status colouring', () => {
	it('marks a failing node with a failing/red marker', () => {
		const { container } = mountStatus('failing');
		const el = statusEl(container, 'failing');
		expect(el).toBeTruthy();
		expect(el?.className).toMatch(/red/);
	});

	it('marks a passing node with a passing/green marker', () => {
		const { container } = mountStatus('passing');
		const el = statusEl(container, 'passing');
		expect(el).toBeTruthy();
		expect(el?.className).toMatch(/green/);
	});

	it('marks an unknown node with neither a passing nor failing colour marker', () => {
		const { container } = mountStatus('unknown');
		const el = statusEl(container, 'unknown');
		expect(el).toBeTruthy();
		expect(el?.className).not.toMatch(/red|green/);
	});
});

function testResult(nodeId: string, outcome: TestOutcome): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-28T00:00:03.000Z',
		sessionId: 'sess-test',
		type: 'test.result',
		payload: { nodeId, testId: 't1', outcome, sessionId: 'sess-test' }
	};
}

describe('Graph.svelte live status recolour', () => {
	it('recolours a rendered node to failing when a test.result fail arrives', async () => {
		ingest(snapshot([fileNode])); // status 'unknown'
		const { container } = render(Graph);
		await tick();
		await waitFor(() => expect(statusEl(container, 'unknown')).toBeTruthy());

		// applyEvent (P5-1) folds the outcome onto the node's status; the canvas recolours
		// with no extra wiring once buildHierarchy threads status into node data.
		ingest(testResult('file:src/x.rs', 'fail'));
		await waitFor(() => expect(statusEl(container, 'failing')).toBeTruthy());
		expect(statusEl(container, 'unknown')).toBeNull();
	});
});

// P6-4: render hot edges. A data-flow edge between two visible roots renders with
// the `animated` dash cue; a `hot_edge` enter folds `hot: true` onto it (P6-1's
// reducer) and the canvas gains the dedicated `hot-edge` overlay, reverting on
// `exit` — all without disturbing the `animated` cue.
const dataEdge: Edge = {
	id: 'e:fn:src/x.rs:a->fn:src/x.rs:b:data_flows_from',
	source: 'fn:src/x.rs:a',
	target: 'fn:src/x.rs:b',
	kind: 'data_flows_from',
	hot: false
};

function hotEdge(edgeId: string, state: 'enter' | 'exit'): EventEnvelope {
	return {
		v: 1,
		ts: '2026-06-29T00:00:00.000Z',
		sessionId: 'sess-test',
		type: 'hot_edge',
		payload: { edgeId, state, sessionId: 'sess-test', ts: '2026-06-29T00:00:00.000Z' }
	};
}

describe('Graph.svelte hot edge rendering', () => {
	it('adds the hot overlay on a hot_edge enter, removes it on exit, leaving animated unchanged', async () => {
		ingest(snapshotWith([fnA, fnB], [dataEdge]));
		const { container } = render(Graph);
		await tick();

		// The data-flow edge renders cold: animated dash present, no hot overlay.
		await waitFor(() =>
			expect(container.querySelectorAll('.svelte-flow__edge.animated').length).toBe(1)
		);
		expect(container.querySelector('.svelte-flow__edge.hot-edge')).toBeNull();

		// hot_edge enter → the edge gains the dedicated hot overlay; animated untouched.
		ingest(hotEdge(dataEdge.id, 'enter'));
		await waitFor(() =>
			expect(container.querySelector('.svelte-flow__edge.hot-edge')).toBeTruthy()
		);
		expect(container.querySelectorAll('.svelte-flow__edge.animated').length).toBe(1);

		// hot_edge exit → overlay reverts; the animated cue is still unchanged.
		ingest(hotEdge(dataEdge.id, 'exit'));
		await waitFor(() => expect(container.querySelector('.svelte-flow__edge.hot-edge')).toBeNull());
		expect(container.querySelectorAll('.svelte-flow__edge.animated').length).toBe(1);
	});
});

// P9-6 (RED): the connection-status indicator. GREEN adds the `connectionStatus`
// writable store to ws.ts (`'connecting' | 'open' | 'reconnecting' | 'closed'`) and a
// small badge in Graph.svelte gated on it, carrying data-testid="connection-status".
// The badge is visible while the socket is not open (so the user sees the resync in
// flight) and absent once the socket recovers. RED until both exist.
describe('Graph.svelte connection-status reconnecting indicator', () => {
	it('shows a "reconnecting" indicator while disconnected and hides it once open', async () => {
		const screen = render(Graph);
		await tick();

		WS.connectionStatus.set('reconnecting');
		await tick();
		const badge = await screen.findByTestId('connection-status');
		expect(badge.textContent).toMatch(/reconnect/i);

		WS.connectionStatus.set('open');
		await tick();
		expect(screen.queryByTestId('connection-status')).toBeNull();
	});
});

// P9-6 fix: the REAL toggle() collapse path must prune the collapsed node's id AND
// its transitive descendant ids from the render-side `expanded` open set. Driven end
// to end through the component (expand file → expand fn → collapse file) and read back
// via a `bind:expanded` harness. Before the fix toggle() deleted only the collapsed
// id, leaving `fn:src/x.rs:alpha` a stale entry a reconnect resync would re-expand
// into an invisible orphan.
describe('Graph.svelte collapse prunes descendants from the open set (P9-6)', () => {
	it('expand file → expand fn → collapse file leaves neither the file id nor its orphaned fn descendant open', async () => {
		// Store holds the full file→fn→var tree so descendantIds can walk it at collapse time.
		ingest(snapshot([fileNode, fnNode, varNode]));
		let open = new Set<string>();
		const onExpand = vi.fn();
		const screen = render(GraphExpandedHarness, {
			props: { onExpand, report: (s: Set<string>) => (open = s) }
		});
		await tick();

		// Expand the file → its function child renders and the file id enters the open set.
		await fireEvent.click(await screen.findByTestId(toggleId('file:src/x.rs')));
		expect(await screen.findByText('alpha')).toBeTruthy();
		// Expand the function → the fn id enters the open set too (nested expansion).
		await fireEvent.click(await screen.findByTestId(toggleId('fn:src/x.rs:alpha')));
		await tick();
		expect(open.has('file:src/x.rs')).toBe(true);
		expect(open.has('fn:src/x.rs:alpha')).toBe(true);

		// Collapse the file: toggle() must drop the file id AND its orphaned fn descendant.
		await fireEvent.click(await screen.findByTestId(toggleId('file:src/x.rs')));
		await tick();
		expect(open.has('file:src/x.rs')).toBe(false);
		expect(open.has('fn:src/x.rs:alpha')).toBe(false);
		expect(open.size).toBe(0);

		// And every surviving open id still exists in the (collapsed) store — no orphans.
		const state = get(graphStore);
		for (const id of open) expect(state.nodes.has(id)).toBe(true);
	});
});
