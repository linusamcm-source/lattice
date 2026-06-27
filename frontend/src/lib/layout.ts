/**
 * Deterministic two-tier layout for the Phase 0 render model.
 *
 * The CLV {@link Node} carries no coordinates, but SvelteFlow requires a
 * `position` per node. {@link buildTwoTier} projects the flat node list onto a
 * two-column layout: every `file` node in a left column, and each file's direct
 * `function` children offset to the right. Ids are sorted so the layout is stable
 * across re-renders. No expand/collapse — that is Phase 1.
 *
 * @module
 */

import type { Node as FlowNode } from '@xyflow/svelte';
import type { Node } from './types';

/** Vertical spacing between stacked rows, in pixels. */
const ROW_HEIGHT = 120;

/** Horizontal offset of the `function` column from the `file` column, in pixels. */
const CHILD_COLUMN_X = 280;

function compareId(a: string, b: string): number {
	return a < b ? -1 : a > b ? 1 : 0;
}

/**
 * Build the flat two-tier SvelteFlow node list from the CLV node store.
 *
 * Renders every `file` node plus each of its direct `function` children (a node
 * whose `parentId` equals a present file node's id). Files stack in a column at
 * `x = 0`; a file's functions sit at `x = 280`, stepped down so a file and its
 * children never overlap. The rendered label is carried in `data.label` for the
 * default SvelteFlow node renderer.
 *
 * @param graphNodes - all current CLV nodes from the `nodes` store.
 * @returns positioned SvelteFlow nodes for the file/function tiers only.
 */
export function buildTwoTier(graphNodes: Node[]): FlowNode[] {
	const files = graphNodes.filter((n) => n.type === 'file').sort((a, b) => compareId(a.id, b.id));

	const flow: FlowNode[] = [];
	let cursor = 0;

	for (const file of files) {
		flow.push({
			id: file.id,
			type: 'default',
			position: { x: 0, y: cursor * ROW_HEIGHT },
			data: { label: file.label }
		});

		const children = graphNodes
			.filter((n) => n.type === 'function' && n.parentId === file.id)
			.sort((a, b) => compareId(a.id, b.id));

		children.forEach((fn, i) => {
			flow.push({
				id: fn.id,
				type: 'default',
				position: { x: CHILD_COLUMN_X, y: (cursor + i) * ROW_HEIGHT },
				data: { label: fn.label }
			});
		});

		cursor += Math.max(1, children.length);
	}

	return flow;
}
