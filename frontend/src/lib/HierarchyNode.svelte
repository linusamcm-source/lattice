<!--
	HierarchyNode.svelte — custom SvelteFlow node for the lazy hierarchy canvas.

	Renders a CLV node's label and, when the node has children, an
	expand/collapse button. Clicking the button calls `data.onToggle(id)`, which
	`Graph.svelte` injects to flip the node's id in its expansion set (and so
	request or discard the node's subtree). Source/target handles let `contains`
	edges attach. The button carries the `nodrag` class so clicking it never
	starts a SvelteFlow node drag.

	@component
-->
<script lang="ts">
	import { Handle, Position, type NodeProps, type Node as FlowNode } from '@xyflow/svelte';
	import type { HierarchyNodeData } from './layout';

	/** Layout data plus the toggle callback injected by `Graph.svelte`. */
	type NodeData = HierarchyNodeData & { onToggle: (nodeId: string) => void };

	let { id, data }: NodeProps<FlowNode<NodeData, 'hierarchy'>> = $props();
</script>

<div
	class="flex items-center gap-2 rounded-md border border-neutral-300 bg-white px-3 py-2 text-sm text-neutral-900 shadow-sm dark:border-neutral-700 dark:bg-neutral-900 dark:text-neutral-100"
>
	<span>{data.label}</span>
	{#if data.expandable}
		<button
			type="button"
			class="nodrag rounded border border-neutral-300 px-1.5 leading-none text-neutral-500 hover:text-neutral-900 dark:border-neutral-700 dark:text-neutral-400 dark:hover:text-neutral-100"
			data-testid={`toggle-${id}`}
			aria-label={`${data.expanded ? 'Collapse' : 'Expand'} ${data.label}`}
			onclick={() => data.onToggle(id)}
		>
			{data.expanded ? '−' : '+'}
		</button>
	{/if}
</div>
<Handle type="target" position={Position.Left} />
<Handle type="source" position={Position.Right} />
