<!--
	HierarchyNode.harness.svelte — test-only fixture.

	`HierarchyNode` renders `<Handle>`s, which require the node-id context that
	only SvelteFlow's internal `NodeWrapper` provides — a bare `SvelteFlowProvider`
	throws "Handle must be used within a Custom Node component". This harness mounts
	the component under test inside a one-node `<SvelteFlow>` (a provider + node
	wrapper) so component tests can assert on its `title` / `data-testid` output in
	isolation. Not collected by Vitest (it is a `.svelte`, not a `.test.ts`).

	@component
-->
<script lang="ts">
	import {
		SvelteFlow,
		Background,
		type Node as FlowNode,
		type Edge as FlowEdge
	} from '@xyflow/svelte';
	import '@xyflow/svelte/dist/style.css';
	import HierarchyNode from './HierarchyNode.svelte';
	import type { HierarchyNodeData } from './layout';

	/** Layout data plus the toggle callback `Graph.svelte` injects at render time. */
	type NodeData = HierarchyNodeData & { onToggle: (nodeId: string) => void };

	let { id, data }: { id: string; data: NodeData } = $props();

	const nodeTypes = { hierarchy: HierarchyNode };
	let nodes = $state.raw<FlowNode[]>([{ id, type: 'hierarchy', position: { x: 0, y: 0 }, data }]);
	let edges = $state.raw<FlowEdge[]>([]);
</script>

<div style="width: 400px; height: 300px;">
	<SvelteFlow {nodeTypes} bind:nodes bind:edges>
		<Background />
	</SvelteFlow>
</div>
