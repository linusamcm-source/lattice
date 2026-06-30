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

	P4-4 draws the graph's edges (`buildEdges`): an edge is rendered only when both
	its endpoints are currently visible (in the `buildHierarchy` output), so an
	edge drops automatically when a parent collapses. Two independent toggles —
	**Control flow** (`calls`) and **Data flow** (`param_source`/`data_flows_from`),
	both default on — include/exclude each edge class; `contains` is never drawn.

	P6-4 renders hot edges: an edge whose store `hot` flag is true (a live runtime
	call path, flipped by `hot_edge` events) gains a dedicated `hot-edge` overlay
	(red pulse, `app.css`) on top of its kind colour. Because the flow edges are
	rebuilt from the `edges` store, this needs no extra wiring here — `buildEdges`
	appends the class and the canvas recolours. Hot is independent of the data-flow
	`animated` dash cue and of the control/data-flow toggles: a filtered-out edge
	stays hidden even when hot, and the overlay reverts the moment it goes cold.

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
	import { nodes, edges, graphStore, requestExpand, collapse } from './ws';
	import { buildHierarchy, buildEdges } from './layout';
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
	/** Whether `calls` (control-flow) edges are drawn. Toggled by the user; default on. */
	let controlFlow = $state(true);
	/** Whether `param_source`/`data_flows_from` (data-flow) edges are drawn. Default on. */
	let dataFlow = $state(true);
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

	// The set of node ids actually on the canvas — exactly the nodes
	// `buildHierarchy` emitted. Collapsing a parent shrinks this set, so its
	// edges drop out of `buildEdges` automatically (lazy discipline).
	let visibleNodeIds = $derived(new Set(flowNodes.map((node) => node.id)));

	// Recompute the visible edges whenever the edge store, the visible-node set,
	// or either flow-class toggle changes. An edge is drawn only when both its
	// endpoints are visible and its flow class is enabled.
	$effect(() => {
		flowEdges = buildEdges($edges, visibleNodeIds, { controlFlow, dataFlow });
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
			<fieldset
				class="absolute left-3 top-3 z-10 flex flex-col gap-1 rounded-md border border-neutral-300 bg-white/90 p-2 text-xs text-neutral-900 shadow-sm backdrop-blur dark:border-neutral-700 dark:bg-neutral-900/90 dark:text-neutral-100"
			>
				<legend class="px-1 text-[0.65rem] font-medium uppercase tracking-wide text-neutral-500">
					Edges
				</legend>
				<label class="flex items-center gap-2">
					<input type="checkbox" class="accent-sky-500" bind:checked={controlFlow} />
					Control flow
				</label>
				<label class="flex items-center gap-2">
					<input type="checkbox" class="accent-amber-500" bind:checked={dataFlow} />
					Data flow
				</label>
			</fieldset>
		{/if}
	</div>
	<Sidebar selected={selectedNode} />
</div>
