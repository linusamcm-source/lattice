<!-- P1-4 index route: the lazy hierarchy SvelteFlow render, wired to the live WS socket. -->
<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import Graph from '$lib/Graph.svelte';
	import { connect, type WsClient } from '$lib/ws';

	/** Dev WebSocket endpoint. Uses 127.0.0.1 (not `localhost`) to match the
	 * backend's IPv4 bind — `localhost` can resolve to IPv6 `::1` and fail. */
	const WS_URL = 'ws://127.0.0.1:7000';

	let client: WsClient | undefined;
	let socket = $state<WebSocket | undefined>(undefined);

	// Browser-only: the route is prerendered, so the socket opens after hydration.
	onMount(() => {
		client = connect(WS_URL);
		socket = client.socket;
	});

	onDestroy(() => client?.close());
</script>

<main class="relative h-screen w-screen">
	<!-- Keeps Tailwind's JIT emitting `text-red-500` (P0-7's built-CSS assertion). -->
	<header class="absolute left-3 top-3 z-10 text-red-500">Lattice</header>
	<Graph {socket} />
</main>
