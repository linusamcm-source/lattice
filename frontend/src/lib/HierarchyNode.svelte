<!--
	HierarchyNode.svelte — custom SvelteFlow node for the lazy hierarchy canvas.

	Renders a CLV node's label and, when the node has children, an
	expand/collapse button. The label is a selection affordance: clicking it calls
	`data.onSelect(id)`, which `Graph.svelte` threads in to open the node in the
	selection sidebar. The node's documentation (`data.docs`) is bound to a `title`
	attribute so hovering any tier (file/function/variable) shows its description as
	a tooltip. The node is colour-coded by its live `data.status` (SPEC §9.6): the
	`STATUS_NODE_CLASS` mapping drives the border/background (green passing, red
	failing, pulsing running, grey stale, hatched error, neutral unknown), and a
	`data-status` attribute exposes the raw status for tests/styling. The node's
	structural `data.type` adds a second, independent cue via `TYPE_NODE_CLASS`
	(only `agent` nodes are styled distinctly today — a dashed violet border/ring),
	exposed as a `data-type` attribute. Accessibility: when SvelteFlow `selected`s the
	node (the agent → code drill-down highlight in `Graph.svelte`), the select button
	carries `aria-current="true"` so the highlight is exposed to assistive tech; and
	an `agent`-type node folds an "agent" qualifier into its accessible name so screen
	readers can tell agent nodes from code nodes. Because
	`status` is threaded straight from the store, a `test.result`/`status.update`
	event recolours the node live. Clicking the expand button calls `data.onToggle(id)` (injected by
	`Graph.svelte` to flip the node's id in its expansion set, requesting or
	discarding its subtree) and first `stopPropagation`s so expanding never also
	selects. Source/target handles let `contains` edges attach. Both buttons carry
	the `nodrag` class so clicking them never starts a SvelteFlow node drag.

	@component
-->
<script lang="ts">
	import { Handle, Position, type NodeProps, type Node as FlowNode } from '@xyflow/svelte';
	import { STATUS_NODE_CLASS, TYPE_NODE_CLASS, type HierarchyNodeData } from './layout';

	/** Layout/selection data plus the toggle callback injected by `Graph.svelte`. */
	type NodeData = HierarchyNodeData & { onToggle: (nodeId: string) => void };

	let { id, data, selected }: NodeProps<FlowNode<NodeData, 'hierarchy'>> = $props();
</script>

<div
	class={`flex items-center gap-2 rounded-md border px-3 py-2 text-sm text-neutral-900 shadow-sm dark:text-neutral-100 ${STATUS_NODE_CLASS[data.status]} ${data.type ? TYPE_NODE_CLASS[data.type] : ''}`}
	data-status={data.status}
	data-type={data.type}
	title={data.docs}
>
	<button
		type="button"
		class="nodrag flex-1 text-left"
		data-testid={`select-${id}`}
		aria-current={selected ? 'true' : undefined}
		aria-label={data.type === 'agent' ? `${data.label}, agent` : undefined}
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
