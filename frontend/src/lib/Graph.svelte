<!--
	Graph.svelte — P1-4 lazy hierarchy SvelteFlow canvas.

	On mount the canvas shows only top-level (root) CLV nodes from the `nodes`
	store. Each node carries an expand/collapse affordance; expanding a node adds
	its id to an explicit `expanded` Set<string> and invokes `onExpand` (default:
	`requestExpand` over the injected `socket`), and once the `subtree` reply has
	merged the children into the store they render. Collapsing removes the id from
	the set and calls `collapse`, discarding the node's transitive descendants
	from the store. `expanded` is the render-side zoom gate: a function's
	`variable` children render only when the function's id is in the set, even if
	those variables already live in the store. The canvas mounts only after
	`onMount` so SvelteKit prerender/SSR never instantiates the browser-only
	SvelteFlow component.

	@component
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
	import { nodes, graphStore, requestExpand, collapse } from './ws';
	import { buildHierarchy } from './layout';
	import HierarchyNode from './HierarchyNode.svelte';
	import Sidebar from './Sidebar.svelte';

	/** Public props for the lazy hierarchy canvas. */
	interface GraphProps {
		/**
		 * Live WebSocket used by the default {@link GraphProps.onExpand} handler to
		 * request a node's subtree. The index route wires the connected socket;
		 * tests that inject `onExpand` can omit it.
		 */
		socket?: WebSocket;
		/**
		 * Expand handler invoked with a node's id when the user expands it. Defaults
		 * to `requestExpand(socket, nodeId)` against {@link GraphProps.socket} so the
		 * backend replies with that node's subtree; tests inject a spy. Collapsing is
		 * always handled internally via `collapse` and never calls this.
		 */
		onExpand?: (nodeId: string) => void;
	}

	let { socket, onExpand }: GraphProps = $props();

	/** Registers the `hierarchy` custom node so labels carry an expand affordance. */
	const nodeTypes = { hierarchy: HierarchyNode };

	let mounted = $state(false);
	/** Ids of nodes whose children are currently revealed (the render-side zoom gate). */
	let expanded = $state(new Set<string>());
	/** Id of the node whose details are shown in the sidebar (`undefined` = none). */
	let selected = $state<string | undefined>(undefined);
	let flowNodes = $state.raw<FlowNode[]>([]);
	let flowEdges = $state.raw<FlowEdge[]>([]);

	// The selected node, looked up live from the store, so a `node.upsert` that
	// changes its docs flows straight through to the sidebar.
	let selectedNode = $derived(
		selected === undefined ? undefined : $nodes.find((node) => node.id === selected)
	);

	onMount(() => {
		mounted = true;
	});

	/**
	 * Select a node, opening it in the sidebar.
	 *
	 * @param nodeId - the id of the node whose content region was activated.
	 */
	function select(nodeId: string): void {
		selected = nodeId;
	}

	/**
	 * Toggle a node's expansion. Expanding reveals the node's children and, by
	 * default, requests its subtree over the live socket; collapsing hides them and
	 * discards the node's transitive descendants from the store to bound memory.
	 *
	 * @param nodeId - the id of the node whose affordance was activated.
	 */
	function toggle(nodeId: string): void {
		const next = new Set(expanded);
		if (next.has(nodeId)) {
			next.delete(nodeId);
			expanded = next;
			graphStore.update((state) => collapse(state, nodeId));
		} else {
			next.add(nodeId);
			expanded = next;
			if (onExpand) onExpand(nodeId);
			else if (socket) requestExpand(socket, nodeId);
		}
	}

	// Recompute the visible, positioned hierarchy whenever the store or the
	// expansion set changes, injecting the toggle callback into each node's data.
	// Children only reach the canvas when their parent id is in `expanded`.
	$effect(() => {
		flowNodes = buildHierarchy($nodes, expanded, select).map((node) => ({
			...node,
			data: { ...node.data, onToggle: toggle }
		}));
	});
</script>

<div class="flex h-full w-full">
	<div class="relative h-full flex-1">
		{#if mounted}
			<SvelteFlow
				{nodeTypes}
				bind:nodes={flowNodes}
				bind:edges={flowEdges}
				colorMode="light"
				fitView
			>
				<Background />
			</SvelteFlow>
		{/if}
	</div>
	<Sidebar selected={selectedNode} />
</div>
