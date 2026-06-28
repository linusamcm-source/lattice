import { describe, it, expect } from 'vitest';
import { buildHierarchy, type HierarchyNodeData } from './layout';
import type { Node } from './types';

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
});
