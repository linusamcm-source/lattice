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

import type { Node as FlowNode } from '@xyflow/svelte';
import type { Node } from './types';

/** Vertical spacing between stacked rows, in pixels. */
const ROW_HEIGHT = 120;

/** Horizontal offset between adjacent depth columns, in pixels. */
const COLUMN_WIDTH = 280;

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
