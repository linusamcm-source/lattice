import { describe, it, expect } from 'vitest';
import { buildHierarchy, buildEdges, type HierarchyNodeData } from './layout';
import type { Node, Edge, EdgeKind } from './types';

const file: Node = {
	id: 'file:src/x.rs',
	type: 'file',
	label: 'x.rs',
	parentId: null,
	childIds: ['fn:src/x.rs:alpha'],
	status: 'unknown'
};

const fn: Node = {
	id: 'fn:src/x.rs:alpha',
	type: 'function',
	label: 'alpha',
	parentId: 'file:src/x.rs',
	childIds: ['var:src/x.rs:alpha:y'],
	status: 'unknown'
};

const variable: Node = {
	id: 'var:src/x.rs:alpha:y',
	type: 'variable',
	label: 'y',
	parentId: 'fn:src/x.rs:alpha',
	childIds: [],
	status: 'unknown'
};

const data = (flow: { data: Record<string, unknown> }): HierarchyNodeData =>
	flow.data as HierarchyNodeData;

describe('buildHierarchy', () => {
	it('renders only root nodes when nothing is expanded', () => {
		const flow = buildHierarchy([file, fn, variable], new Set());
		expect(flow.map((n) => n.id)).toEqual(['file:src/x.rs']);
		expect(data(flow[0]).label).toBe('x.rs');
		expect(data(flow[0]).expandable).toBe(true);
		expect(data(flow[0]).expanded).toBe(false);
		expect(flow[0].position).toEqual({ x: 0, y: 0 });
	});

	it('reveals direct children of an expanded node, offset one column right', () => {
		const flow = buildHierarchy([file, fn, variable], new Set(['file:src/x.rs']));
		expect(flow.map((n) => n.id)).toEqual(['file:src/x.rs', 'fn:src/x.rs:alpha']);
		const child = flow[1];
		expect(child.position.x).toBeGreaterThan(flow[0].position.x);
		expect(child.position.y).toBeGreaterThan(flow[0].position.y);
		expect(data(child).expanded).toBe(false);
	});

	it('zoom-gates grandchildren until the parent function is also expanded', () => {
		const fileOnly = buildHierarchy([file, fn, variable], new Set(['file:src/x.rs']));
		expect(fileOnly.map((n) => n.id)).not.toContain('var:src/x.rs:alpha:y');

		const both = buildHierarchy(
			[file, fn, variable],
			new Set(['file:src/x.rs', 'fn:src/x.rs:alpha'])
		);
		expect(both.map((n) => n.id)).toEqual([
			'file:src/x.rs',
			'fn:src/x.rs:alpha',
			'var:src/x.rs:alpha:y'
		]);
		const grandchild = both[2];
		expect(grandchild.position.x).toBeGreaterThan(both[1].position.x);
		expect(data(grandchild).expandable).toBe(false);
	});

	it('does not mark a childless root node as expandable', () => {
		const lonely: Node = { ...file, childIds: [] };
		const flow = buildHierarchy([lonely], new Set());
		expect(data(flow[0]).expandable).toBe(false);
	});

	// P5-5: status threaded into node data so the canvas can colour by status.
	it('copies a node status into its node data', () => {
		const failing: Node = { ...file, status: 'failing' };
		const flow = buildHierarchy([failing], new Set());
		expect(data(flow[0]).status).toBe('failing');
	});

	it('threads the default unknown status through to node data', () => {
		const flow = buildHierarchy([file], new Set());
		expect(data(flow[0]).status).toBe('unknown');
	});
});

// P4-4: edge rendering + kind colour/class + control/data-flow filter.
const mkEdge = (id: string, source: string, target: string, kind: EdgeKind): Edge => ({
	id,
	source,
	target,
	kind,
	hot: false
});

const BOTH_ON = { controlFlow: true, dataFlow: true };
const both = new Set(['fn:a', 'fn:b']);
const classOf = (edge: { class?: unknown }): string => `${edge.class ?? ''}`;

describe('buildEdges', () => {
	it('renders a calls edge between visible endpoints as a control-flow edge', () => {
		const out = buildEdges([mkEdge('e:fn:a->fn:b:calls', 'fn:a', 'fn:b', 'calls')], both, BOTH_ON);
		expect(out).toHaveLength(1);
		expect(out[0].id).toBe('e:fn:a->fn:b:calls');
		expect(out[0].source).toBe('fn:a');
		expect(out[0].target).toBe('fn:b');
		expect(out[0].data?.flowClass).toBe('control');
		expect(classOf(out[0])).toContain('lattice-edge-control');
	});

	it('renders a param_source edge as a data-flow edge', () => {
		const out = buildEdges(
			[mkEdge('e:fn:b->fn:a:param_source', 'fn:b', 'fn:a', 'param_source')],
			both,
			BOTH_ON
		);
		expect(out).toHaveLength(1);
		expect(out[0].data?.flowClass).toBe('data');
		expect(classOf(out[0])).toContain('lattice-edge-data');
	});

	it('classifies data_flows_from as a data-flow edge', () => {
		const out = buildEdges(
			[mkEdge('e:fn:a->fn:b:data_flows_from', 'fn:a', 'fn:b', 'data_flows_from')],
			both,
			BOTH_ON
		);
		expect(out).toHaveLength(1);
		expect(out[0].data?.flowClass).toBe('data');
	});

	it('omits an edge when either endpoint is not in the visible set', () => {
		const e = mkEdge('e:fn:a->fn:b:calls', 'fn:a', 'fn:b', 'calls');
		expect(buildEdges([e], new Set(['fn:a']), BOTH_ON)).toEqual([]);
		expect(buildEdges([e], new Set(['fn:b']), BOTH_ON)).toEqual([]);
		expect(buildEdges([e], new Set(), BOTH_ON)).toEqual([]);
	});

	it('never draws a contains edge even when both endpoints are visible', () => {
		const e = mkEdge('e:file:x->fn:a', 'file:x', 'fn:a', 'contains');
		expect(buildEdges([e], new Set(['file:x', 'fn:a']), BOTH_ON)).toEqual([]);
	});

	it('excludes calls when controlFlow is off but keeps data-flow edges', () => {
		const calls = mkEdge('e:fn:a->fn:b:calls', 'fn:a', 'fn:b', 'calls');
		const param = mkEdge('e:fn:b->fn:a:param_source', 'fn:b', 'fn:a', 'param_source');
		const out = buildEdges([calls, param], both, { controlFlow: false, dataFlow: true });
		expect(out.map((e) => e.id)).toEqual(['e:fn:b->fn:a:param_source']);
		expect(out[0].data?.flowClass).toBe('data');
	});

	it('excludes data-flow when dataFlow is off but keeps calls edges', () => {
		const calls = mkEdge('e:fn:a->fn:b:calls', 'fn:a', 'fn:b', 'calls');
		const param = mkEdge('e:fn:b->fn:a:param_source', 'fn:b', 'fn:a', 'param_source');
		const flows = mkEdge('e:fn:a->fn:b:data_flows_from', 'fn:a', 'fn:b', 'data_flows_from');
		const out = buildEdges([calls, param, flows], both, { controlFlow: true, dataFlow: false });
		expect(out.map((e) => e.id)).toEqual(['e:fn:a->fn:b:calls']);
		expect(out[0].data?.flowClass).toBe('control');
	});

	it('returns nothing when both toggles are off', () => {
		const calls = mkEdge('e:fn:a->fn:b:calls', 'fn:a', 'fn:b', 'calls');
		const param = mkEdge('e:fn:b->fn:a:param_source', 'fn:b', 'fn:a', 'param_source');
		expect(buildEdges([calls, param], both, { controlFlow: false, dataFlow: false })).toEqual([]);
	});
});
