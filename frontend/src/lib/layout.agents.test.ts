// P8-6 RED — agent-layer render contract for layout.ts.
//
// These tests pin the layout-side contract the P8-6 implementation must satisfy.
// They are RED until the impl adds, to ./layout:
//   - a `type: NodeType` field on `HierarchyNodeData`, populated by `buildHierarchy`;
//   - a `TYPE_NODE_CLASS: Record<NodeType, string>` map (sibling of `STATUS_NODE_CLASS`)
//     giving agent nodes a distinct class;
//   - a `showAgents` gate parameter on `buildHierarchy` (4th positional, default off)
//     that excludes `type === 'agent'` nodes when off and includes them when on;
//   - an exported `flowClassOf` that maps `authored_by` to a non-null `'agent'` flow class
//     (today it is private and returns null for `authored_by`);
//   - an `agent` flag on `EdgeFilter` gating `authored_by` edges in `buildEdges`;
//   - pure drill-down mappings `nodesAuthoredBy(edges, agentId)` and
//     `agentsForNode(edges, nodeId)`;
//   - a `safeColor(input)` sanitiser that rejects non-hex/non-rgb colour strings.
//
// No `any`: every fixture is a typed CLV `Node`/`Edge`; filter literals are `EdgeFilter`.

import { describe, it, expect } from 'vitest';
import {
	buildHierarchy,
	buildEdges,
	flowClassOf,
	TYPE_NODE_CLASS,
	nodesAuthoredBy,
	agentsForNode,
	safeColor,
	type HierarchyNodeData,
	type EdgeFilter
} from './layout';
import type { Node, Edge, EdgeKind } from './types';

const fileNode: Node = {
	id: 'file:src/x.rs',
	type: 'file',
	label: 'x.rs',
	parentId: null,
	childIds: [],
	status: 'unknown'
};

const agentNode: Node = {
	id: 'agent:tdd-green',
	type: 'agent',
	label: 'tdd-green',
	parentId: null,
	childIds: [],
	status: 'running'
};

const data = (flow: { data: Record<string, unknown> }): HierarchyNodeData =>
	flow.data as HierarchyNodeData;

const idsOf = (flow: { id: string }[]): string[] => flow.map((n) => n.id);

// AC1 — node.type is threaded into HierarchyNodeData, and agent nodes get a distinct class.
describe('buildHierarchy threads node.type into node data (P8-6)', () => {
	it('carries an agent node type through to the data block', () => {
		const flow = buildHierarchy([agentNode], new Set(), () => {}, true);
		expect(idsOf(flow)).toEqual(['agent:tdd-green']);
		expect(data(flow[0]).type).toBe('agent');
	});

	it('carries a non-agent node type through to the data block', () => {
		const flow = buildHierarchy([fileNode], new Set(), () => {}, true);
		expect(data(flow[0]).type).toBe('file');
	});
});

describe('TYPE_NODE_CLASS agent styling (P8-6)', () => {
	it('exposes a non-empty class for agent nodes', () => {
		expect(typeof TYPE_NODE_CLASS.agent).toBe('string');
		expect(TYPE_NODE_CLASS.agent.length).toBeGreaterThan(0);
	});

	it('gives agent nodes a class distinct from ordinary code nodes', () => {
		expect(TYPE_NODE_CLASS.agent).not.toBe(TYPE_NODE_CLASS.function);
		expect(TYPE_NODE_CLASS.agent).not.toBe(TYPE_NODE_CLASS.file);
	});
});

// AC2 — agent NODES are gated on the agent-layer toggle.
describe('buildHierarchy agent-layer visibility gate (P8-6)', () => {
	it('excludes agent-type nodes when the agent layer is OFF', () => {
		const flow = buildHierarchy([fileNode, agentNode], new Set(), () => {}, false);
		const ids = idsOf(flow);
		expect(ids).toContain('file:src/x.rs');
		expect(ids).not.toContain('agent:tdd-green');
	});

	it('includes agent-type nodes when the agent layer is ON', () => {
		const flow = buildHierarchy([fileNode, agentNode], new Set(), () => {}, true);
		const ids = idsOf(flow);
		expect(ids).toContain('file:src/x.rs');
		expect(ids).toContain('agent:tdd-green');
	});
});

// AC3 — authored_by EDGES are a non-null 'agent' flow class, gated on filter.agent.
const mkEdge = (id: string, source: string, target: string, kind: EdgeKind): Edge => ({
	id,
	source,
	target,
	kind,
	hot: false
});

const classOf = (edge: { class?: unknown }): string => `${edge.class ?? ''}`;

const authoredBy = mkEdge(
	'e:agent:tdd-green->fn:src/x.rs:alpha:authored_by',
	'agent:tdd-green',
	'fn:src/x.rs:alpha',
	'authored_by'
);
const visible = new Set(['agent:tdd-green', 'fn:src/x.rs:alpha']);

const AGENT_ON: EdgeFilter = { controlFlow: true, dataFlow: true, agent: true };
const AGENT_OFF: EdgeFilter = { controlFlow: true, dataFlow: true, agent: false };

describe('flowClassOf classifies authored_by as an agent flow class (P8-6)', () => {
	it('maps authored_by to a non-null agent class', () => {
		expect(flowClassOf('authored_by')).toBe('agent');
	});

	it('still maps calls to control flow', () => {
		expect(flowClassOf('calls')).toBe('control');
	});
});

describe('buildEdges agent-edge gate (P8-6)', () => {
	it('draws an authored_by edge between visible endpoints when filter.agent is on', () => {
		const out = buildEdges([authoredBy], visible, AGENT_ON);
		expect(out).toHaveLength(1);
		expect(out[0].id).toBe(authoredBy.id);
		expect(out[0].data?.flowClass).toBe('agent');
		expect(classOf(out[0])).toContain('lattice-edge-agent');
	});

	it('omits authored_by edges when filter.agent is off', () => {
		expect(buildEdges([authoredBy], visible, AGENT_OFF)).toEqual([]);
	});

	it('omits an authored_by edge when an endpoint is not visible even with agent on', () => {
		expect(buildEdges([authoredBy], new Set(['fn:src/x.rs:alpha']), AGENT_ON)).toEqual([]);
	});
});

// AC5 — bidirectional drill-down: pure agentId<->code-node mappings over authored_by edges.
describe('nodesAuthoredBy / agentsForNode drill-down mappings (P8-6)', () => {
	// Wire direction (P8-2): authored_by runs code-node `source` → agent `target`.
	const e1 = mkEdge('e:fn:a->agent:tdd-green', 'fn:a', 'agent:tdd-green', 'authored_by');
	const e2 = mkEdge('e:fn:b->agent:tdd-green', 'fn:b', 'agent:tdd-green', 'authored_by');
	const e3 = mkEdge('e:fn:a->agent:reviewer', 'fn:a', 'agent:reviewer', 'authored_by');
	const calls = mkEdge('e:fn:a->fn:b:calls', 'fn:a', 'fn:b', 'calls');
	const edges = [e1, e2, e3, calls];

	it('maps an agentId to the set of code-node ids it authored (authored_by only)', () => {
		expect(nodesAuthoredBy(edges, 'tdd-green')).toEqual(new Set(['fn:a', 'fn:b']));
		expect(nodesAuthoredBy(edges, 'reviewer')).toEqual(new Set(['fn:a']));
	});

	it('returns an empty set for an agent that authored nothing', () => {
		expect(nodesAuthoredBy(edges, 'nobody')).toEqual(new Set<string>());
	});

	it('maps a code-node id to the set of agentIds that authored it (agent: prefix stripped)', () => {
		expect(agentsForNode(edges, 'fn:a')).toEqual(new Set(['tdd-green', 'reviewer']));
		expect(agentsForNode(edges, 'fn:b')).toEqual(new Set(['tdd-green']));
	});

	it('ignores non-authored_by edges in both directions', () => {
		// `calls` fn:a->fn:b must never imply authorship.
		expect(nodesAuthoredBy([calls], 'fn:a')).toEqual(new Set<string>());
		expect(agentsForNode([calls], 'fn:b')).toEqual(new Set<string>());
	});
});

// SECURITY (carried from P8-5 review) — agent.color is sanitised before any style binding.
describe('safeColor sanitises agent colours before style binding (P8-6)', () => {
	it('passes valid hex colours through unchanged', () => {
		expect(safeColor('#2ecc71')).toBe('#2ecc71');
		expect(safeColor('#abc')).toBe('#abc');
	});

	it('passes a valid rgb() colour through unchanged', () => {
		expect(safeColor('rgb(46, 204, 113)')).toBe('rgb(46, 204, 113)');
	});

	it('rejects a CSS-injection payload rather than passing it raw', () => {
		expect(safeColor('red;background:url(x)')).toBeNull();
	});

	it('rejects url() and other non-colour expressions', () => {
		expect(safeColor('url(x)')).toBeNull();
		expect(safeColor('javascript:alert(1)')).toBeNull();
		expect(safeColor('expression(alert(1))')).toBeNull();
	});
});
