<!--
	HierarchyNode.svelte — custom SvelteFlow node for the lazy hierarchy canvas.

	Renders a CLV node's label and, when the node has children, an
	expand/collapse button. The label is a selection affordance: clicking it calls
	`data.onSelect(id)`, which `Graph.svelte` threads in to open the node in the
	selection sidebar. The node's documentation (`data.docs`) is bound to a `title`
	attribute so hovering any tier (file/function/variable) shows its description as
	a tooltip. Clicking the expand button calls `data.onToggle(id)` (injected by
	`Graph.svelte` to flip the node's id in its expansion set, requesting or
	discarding its subtree) and first `stopPropagation`s so expanding never also
	selects. Source/target handles let `contains` edges attach. Both buttons carry
	the `nodrag` class so clicking them never starts a SvelteFlow node drag.

	@component
-->
<script lang="ts">
	import { Handle, Position, type NodeProps, type Node as FlowNode } from '@xyflow/svelte';
	import type { HierarchyNodeData } from './layout';

	/** Layout/selection data plus the toggle callback injected by `Graph.svelte`. */
	type NodeData = HierarchyNodeData & { onToggle: (nodeId: string) => void };

	let { id, data }: NodeProps<FlowNode<NodeData, 'hierarchy'>> = $props();
</script>

<div
	class="flex items-center gap-2 rounded-md border border-neutral-300 bg-white px-3 py-2 text-sm text-neutral-900 shadow-sm dark:border-neutral-700 dark:bg-neutral-900 dark:text-neutral-100"
	title={data.docs}
>
	<button
		type="button"
		class="nodrag flex-1 text-left"
		data-testid={`select-${id}`}
		onclick={() => data.onSelect(id)}
	>
		{data.label}
	</button>
	{#if data.expandable}
		<button
			type="button"
			class="nodrag rounded border border-neutral-300 px-1.5 leading-none text-neutral-500 hover:text-neutral-900 dark:border-neutral-700 dark:text-neutral-400 dark:hover:text-neutral-100"
			data-testid={`toggle-${id}`}
			aria-label={`${data.expanded ? 'Collapse' : 'Expand'} ${data.label}`}
			onclick={(event) => {
				event.stopPropagation();
				data.onToggle(id);
			}}
		>
			{data.expanded ? '−' : '+'}
		</button>
	{/if}
</div>
<Handle type="target" position={Position.Left} />
<Handle type="source" position={Position.Right} />
