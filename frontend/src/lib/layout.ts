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
 * Edges are projected by {@link buildEdges}: each visible CLV {@link Edge}
 * becomes a SvelteFlow edge coloured by flow class, with data-flow edges
 * carrying the `animated` dash cue. A **hot** edge (live runtime call path)
 * additionally gains a dedicated `hot-edge` class for the red pulse overlay
 * (`app.css`) — a separate cue that never disturbs the `animated` flag.
 *
 * @module
 */

import {
	Position,
	type Node as FlowNode,
	type Edge as FlowEdge,
	type NodeHandle
} from '@xyflow/svelte';
import type { Edge, EdgeKind, Node, NodeStatus } from './types';

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
 * `onSelect`, `docs`, and `status`, by contrast, are threaded through
 * {@link buildHierarchy} so `HierarchyNode` can surface a node's documentation
 * (title tooltip), colour it by status, and report selection without
 * `Graph.svelte` re-deriving the data block.
 */
export type HierarchyNodeData = {
	/** Display label (the CLV node's `label`). */
	label: string;
	/** Whether the node has children and so carries an expand/collapse affordance. */
	expandable: boolean;
	/** Whether the node is currently expanded (its children are revealed). */
	expanded: boolean;
	/**
	 * The CLV node's live {@link NodeStatus}, copied straight from the store so the
	 * node recolours (via {@link STATUS_NODE_CLASS}) the moment a `test.result` /
	 * `status.update` event folds a new status onto it — no extra render wiring.
	 */
	status: NodeStatus;
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

/**
 * Status → node-styling Tailwind classes, applied by `HierarchyNode` to colour a
 * node by its live {@link NodeStatus} (SPEC §9.6's visual language):
 *
 * - `passing` → green border/tint,
 * - `failing` → red border/tint,
 * - `running` → a pulsing animation (the non-colour cue) over a sky tint,
 * - `stale` → a grey border/tint,
 * - `error` → a red hatched fill (the `lattice-status-error` rule in `app.css`),
 * - `unknown` → the neutral default (no colour signal).
 *
 * Each entry sets only the border colour, background, and (for `running`/`error`)
 * the animation / hatch overlay; the layout-independent base classes and the text
 * colour live on the node element itself so utilities never conflict. Colours come
 * from Tailwind theme tokens, never hard-coded hex.
 */
export const STATUS_NODE_CLASS: Record<NodeStatus, string> = {
	unknown: 'border-neutral-300 bg-white dark:border-neutral-700 dark:bg-neutral-900',
	passing: 'border-green-500 bg-green-50 dark:border-green-600 dark:bg-green-950',
	failing: 'border-red-500 bg-red-50 dark:border-red-600 dark:bg-red-950',
	running: 'animate-pulse border-sky-500 bg-sky-50 dark:border-sky-600 dark:bg-sky-950',
	stale: 'border-neutral-400 bg-neutral-100 dark:border-neutral-600 dark:bg-neutral-800',
	error: 'lattice-status-error border-red-600 bg-red-50 dark:border-red-700 dark:bg-red-950'
};

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
				status: node.status,
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
	/**
	 * Whether the edge is on the live runtime stack (the CLV {@link Edge.hot} flag,
	 * toggled by `hot_edge` events). When `true` the built edge also carries the
	 * dedicated `hot-edge` class for the hot overlay; mirroring it here lets tests
	 * and any custom edge component read hot without re-parsing the `class` string.
	 * This is a **dedicated** cue, independent of the data-flow `animated` flag.
	 */
	hot: boolean;
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
 * A **hot** edge (CLV {@link Edge.hot} true — a live runtime call path, set by
 * `hot_edge` events) additionally gets the dedicated `hot-edge` class **appended
 * to** its kind colour class (so it keeps that colour and gains the red pulse
 * overlay defined in `app.css`) and `data.hot === true`. Hot is a **separate**
 * cue: it never touches `animated` (a cold data-flow edge stays `animated`, a
 * cold control-flow edge stays not, and hot changes neither) and it never
 * bypasses the visibility / toggle gates above — a filtered-out edge stays out
 * even when hot. The overlay reverts the instant the edge goes cold.
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

		const kindClass =
			flowClass === 'control'
				? 'lattice-edge-control [&_path]:stroke-sky-500'
				: 'lattice-edge-data [&_path]:stroke-amber-500';

		flow.push({
			id: edge.id,
			source: edge.source,
			target: edge.target,
			// Data flow keeps its `animated` dash cue; `hot` deliberately never
			// touches this — the hot overlay is the dedicated `hot-edge` class below.
			animated: flowClass === 'data',
			class: edge.hot ? `${kindClass} hot-edge` : kindClass,
			data: { kind: edge.kind, flowClass, hot: edge.hot }
		});
	}

	return flow;
}
