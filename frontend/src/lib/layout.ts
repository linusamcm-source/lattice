/**
 * Deterministic lazy-hierarchy layout for the Phase 1 render model.
 *
 * The CLV {@link Node} carries no coordinates, but SvelteFlow requires a
 * `position` per node. {@link buildHierarchy} projects the visible slice of the
 * tree — roots, plus the descendants of every currently-expanded node — onto a
 * column-per-depth layout: roots at `x = 0`, an expanded node's children one
 * column to the right, their children a further column right, and so on. Nodes
 * are emitted in a stable pre-order (ids sorted at each level) so the layout is
 * deterministic across re-renders.
 *
 * The `expanded` set is the render-side zoom gate: a node's children are laid
 * out only when its id is present, so a function's `variable` children stay off
 * the canvas until the function is drilled into, even when they already live in
 * the store.
 *
 * @module
 */

import {
	Position,
	type Node as FlowNode,
	type Edge as FlowEdge,
	type NodeHandle
} from '@xyflow/svelte';
import type { Edge, EdgeKind, Node } from './types';

/** Vertical spacing between stacked rows, in pixels. */
const ROW_HEIGHT = 120;

/** Horizontal offset between adjacent depth columns, in pixels. */
const COLUMN_WIDTH = 280;

/** Pre-measurement size hint for a hierarchy node, in pixels. */
const NODE_WIDTH = 200;
const NODE_HEIGHT = 48;

/**
 * Deterministic handle anchors for every hierarchy node: a target on the left
 * edge and a source on the right edge, matching the `<Handle>`s rendered by
 * `HierarchyNode`. Declaring them on the node lets SvelteFlow route edges on the
 * first paint instead of waiting for the async measurement pass; once the node
 * is measured the real handle bounds take over.
 */
const NODE_HANDLES: NodeHandle[] = [
	{ type: 'target', position: Position.Left, x: 0, y: NODE_HEIGHT / 2 },
	{ type: 'source', position: Position.Right, x: NODE_WIDTH, y: NODE_HEIGHT / 2 }
];

/**
 * Per-node payload carried in a SvelteFlow node's `data`, consumed by the
 * `HierarchyNode` custom node component. `onToggle` is injected by
 * `Graph.svelte` at render time and is therefore not part of this layout type;
 * `onSelect` and `docs`, by contrast, are threaded through {@link buildHierarchy}
 * so `HierarchyNode` can surface a node's documentation (title tooltip) and
 * report selection without `Graph.svelte` re-deriving the data block.
 */
export type HierarchyNodeData = {
	/** Display label (the CLV node's `label`). */
	label: string;
	/** Whether the node has children and so carries an expand/collapse affordance. */
	expandable: boolean;
	/** Whether the node is currently expanded (its children are revealed). */
	expanded: boolean;
	/**
	 * The CLV node's documentation, surfaced as a hover tooltip. Absent when the
	 * node carries no extracted docs.
	 */
	docs?: string;
	/**
	 * Selection callback invoked with the node's id when its content region is
	 * activated. Threaded in by `Graph.svelte` so clicking a node opens it in the
	 * selection sidebar.
	 */
	onSelect: (nodeId: string) => void;
};

/** A positioned SvelteFlow node of the custom `hierarchy` type. */
export type HierarchyFlowNode = FlowNode<HierarchyNodeData, 'hierarchy'>;

function compareId(a: string, b: string): number {
	return a < b ? -1 : a > b ? 1 : 0;
}

/**
 * Build the visible, positioned SvelteFlow node list from the CLV node store.
 *
 * Walks the tree in pre-order starting at the roots (`parentId` null/absent),
 * descending into a node's children only when its id is in `expanded`. Each
 * visible node gets its own row (`y = row * ROW_HEIGHT`, top-to-bottom in visit
 * order) and a column by depth (`x = depth * COLUMN_WIDTH`). A node is marked
 * `expandable` when it has any children — either declared via `childIds` or
 * already present in the store — so a lazily-loaded parent still offers an
 * expand affordance before its subtree arrives.
 *
 * @param graphNodes - all current CLV nodes from the `nodes` store.
 * @param expanded - ids of nodes whose children should be revealed (the zoom gate).
 * @param onSelect - selection callback threaded into every node's data block
 *   (defaults to a no-op so layout-only callers need not supply one).
 * @returns positioned SvelteFlow nodes for exactly the visible tiers.
 */
export function buildHierarchy(
	graphNodes: Node[],
	expanded: ReadonlySet<string>,
	onSelect: (nodeId: string) => void = () => {}
): HierarchyFlowNode[] {
	const childrenOf = (id: string): Node[] =>
		graphNodes.filter((n) => n.parentId === id).sort((a, b) => compareId(a.id, b.id));

	const hasChildren = (node: Node): boolean =>
		node.childIds.length > 0 || graphNodes.some((n) => n.parentId === node.id);

	const flow: HierarchyFlowNode[] = [];
	let row = 0;

	const walk = (node: Node, depth: number): void => {
		flow.push({
			id: node.id,
			type: 'hierarchy',
			position: { x: depth * COLUMN_WIDTH, y: row * ROW_HEIGHT },
			initialWidth: NODE_WIDTH,
			initialHeight: NODE_HEIGHT,
			handles: NODE_HANDLES,
			data: {
				label: node.label,
				expandable: hasChildren(node),
				expanded: expanded.has(node.id),
				docs: node.docs,
				onSelect
			}
		});
		row += 1;
		if (expanded.has(node.id)) {
			for (const child of childrenOf(node.id)) walk(child, depth + 1);
		}
	};

	const roots = graphNodes.filter((n) => n.parentId == null).sort((a, b) => compareId(a.id, b.id));
	for (const root of roots) walk(root, 0);

	return flow;
}

/**
 * The two edge classes the canvas renders, keyed off the CLV {@link EdgeKind}:
 * `calls` is **control flow**, and `param_source` / `data_flows_from` are
 * **data flow**. `contains` (and every other kind) is not a flow class and is
 * never drawn — containment is already conveyed by the column layout.
 */
export type EdgeFlowClass = 'control' | 'data';

/**
 * Independent visibility toggles for the two edge classes. Both default on in
 * `Graph.svelte`; flipping one to `false` drops that class of edge from the
 * canvas without touching the other.
 */
export interface EdgeFilter {
	/** When `false`, `calls` (control-flow) edges are excluded. */
	controlFlow: boolean;
	/** When `false`, `param_source` / `data_flows_from` (data-flow) edges are excluded. */
	dataFlow: boolean;
}

/**
 * Per-edge payload carried in a SvelteFlow edge's `data`, so the flow class is
 * inspectable without re-parsing the `class` string (used by tests and any
 * future custom edge component).
 */
export type HierarchyEdgeData = {
	/** The originating CLV edge kind. */
	kind: EdgeKind;
	/** Which toggle class the edge belongs to. */
	flowClass: EdgeFlowClass;
};

/** A SvelteFlow edge carrying its {@link HierarchyEdgeData} flow-class marker. */
export type HierarchyFlowEdge = FlowEdge<HierarchyEdgeData>;

/** Map a CLV edge kind to its flow class, or `null` when the kind is never drawn. */
function flowClassOf(kind: EdgeKind): EdgeFlowClass | null {
	switch (kind) {
		case 'calls':
			return 'control';
		case 'param_source':
		case 'data_flows_from':
			return 'data';
		default:
			// `contains` and any other relationship are conveyed structurally, not drawn.
			return null;
	}
}

/**
 * Build the visible SvelteFlow edge list from the CLV edge store.
 *
 * An edge is rendered only when **all** of the following hold, so the canvas
 * stays in lockstep with the lazy node hierarchy:
 *
 * 1. Its `kind` maps to a flow class via {@link flowClassOf} (`calls` →
 *    control, `param_source` / `data_flows_from` → data). `contains` and every
 *    other kind are skipped — containment is shown by the column layout.
 * 2. That flow class is enabled in `filter` (`controlFlow` / `dataFlow`), which
 *    the two independent toggles drive.
 * 3. **Both** `source` and `target` are present in `visibleNodeIds` — the set of
 *    node ids `buildHierarchy` actually emitted. Collapsing a parent removes its
 *    descendants from that set, so their edges drop out automatically with no
 *    special-casing.
 *
 * Each rendered edge is colour-/class-keyed by flow class: a semantic `class`
 * marker (`lattice-edge-control` / `lattice-edge-data`) plus a Tailwind stroke
 * utility, `animated` for data flow (a non-colour cue), and a typed
 * {@link HierarchyEdgeData} `data` block.
 *
 * @param graphEdges - all current CLV edges from the `edges` store.
 * @param visibleNodeIds - ids of the nodes currently on the canvas (the zoom gate).
 * @param filter - the control-/data-flow toggle state.
 * @returns SvelteFlow edges for exactly the visible, enabled flow edges.
 */
export function buildEdges(
	graphEdges: Edge[],
	visibleNodeIds: ReadonlySet<string>,
	filter: EdgeFilter
): HierarchyFlowEdge[] {
	const flow: HierarchyFlowEdge[] = [];

	for (const edge of graphEdges) {
		const flowClass = flowClassOf(edge.kind);
		if (flowClass === null) continue;
		if (flowClass === 'control' && !filter.controlFlow) continue;
		if (flowClass === 'data' && !filter.dataFlow) continue;
		if (!visibleNodeIds.has(edge.source) || !visibleNodeIds.has(edge.target)) continue;

		flow.push({
			id: edge.id,
			source: edge.source,
			target: edge.target,
			animated: flowClass === 'data',
			class:
				flowClass === 'control'
					? 'lattice-edge-control [&_path]:stroke-sky-500'
					: 'lattice-edge-data [&_path]:stroke-amber-500',
			data: { kind: edge.kind, flowClass }
		});
	}

	return flow;
}
