<!--
	Graph.svelte — Phase 0 two-tier SvelteFlow canvas.

	Subscribes to the `nodes` store and renders a flat two-tier view (file nodes and
	their direct function children) via the deterministic `buildTwoTier` layout. The
	SvelteFlow canvas is mounted only after `onMount` so SvelteKit prerender/SSR never
	instantiates it. No expand/collapse — that is Phase 1.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import {
		SvelteFlow,
		Background,
		type Node as FlowNode,
		type Edge as FlowEdge
	} from '@xyflow/svelte';
	import '@xyflow/svelte/dist/style.css';
	import { nodes } from './ws';
	import { buildTwoTier } from './layout';

	let mounted = $state(false);
	let flowNodes = $state.raw<FlowNode[]>([]);
	let flowEdges = $state.raw<FlowEdge[]>([]);

	onMount(() => {
		mounted = true;
	});

	// Recompute the positioned SvelteFlow nodes whenever the CLV node store changes.
	$effect(() => {
		flowNodes = buildTwoTier($nodes);
	});
</script>

<div class="h-full w-full">
	{#if mounted}
		<SvelteFlow bind:nodes={flowNodes} bind:edges={flowEdges} colorMode="light" fitView>
			<Background />
		</SvelteFlow>
	{/if}
</div>
