<!-- P0-8 index route: the SvelteFlow two-tier live render fed by the WS store. -->
<script lang="ts">
	import Graph from '$lib/Graph.svelte';
</script>

<main class="relative h-screen w-screen">
	<!-- Keeps Tailwind's JIT emitting `text-red-500` (P0-7's built-CSS assertion). -->
	<header class="absolute left-3 top-3 z-10 text-red-500">Lattice</header>
	<Graph />
</main>
