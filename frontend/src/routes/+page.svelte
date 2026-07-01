<!-- P1-4 index route: the lazy hierarchy SvelteFlow render, wired to a resilient WS client. -->
<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import Graph from '$lib/Graph.svelte';
	import { connect, deriveWsUrl, type WsClient } from '$lib/ws';

	/** WebSocket endpoint derived from the serving origin so the bundle the `lattice`
	 * binary serves reconnects to whatever host/port served it; off-browser (SSR) it
	 * falls back to the dev default (`ws://127.0.0.1:7000`). */
	const WS_URL = deriveWsUrl(typeof window !== 'undefined' ? window.location : undefined);

	// The stable resilient client handle. Held (not its one-shot `socket`) so expansions
	// always reach the current live socket even after an auto-reconnect swaps it out.
	let client: WsClient | undefined;
	// Mirrors Graph's render-side open-node set (bound below), so the client can re-read
	// it fresh on every re-open and re-expand exactly the nodes still open.
	let expanded = $state(new Set<string>());

	// Browser-only: the route is prerendered, so the socket opens after hydration. The
	// client auto-reconnects with backoff and resyncs (snapshot + re-expand) on re-open.
	onMount(() => {
		client = connect(WS_URL, { getExpandedNodes: () => expanded });
	});

	onDestroy(() => client?.close());
</script>

<main class="relative h-screen w-screen">
	<!-- Keeps Tailwind's JIT emitting `text-red-500` (P0-7's built-CSS assertion). -->
	<header class="absolute left-3 top-3 z-10 text-red-500">Lattice</header>
	<Graph bind:expanded onExpand={(nodeId) => client?.requestExpand(nodeId)} />
</main>
