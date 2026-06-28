<!--
	Sidebar.svelte — selection inspector for the lazy hierarchy canvas.

	Shows the currently selected CLV node's label and its extracted documentation.
	When the node carries no docs (absent or empty) it renders an explicit empty
	state instead of the literal `undefined`; when nothing is selected it renders a
	hint prompting the user to pick a node. `Graph.svelte` looks the selected node
	up from the `nodes` store and passes it in, so a `node.upsert` that changes the
	selected node's docs flows straight through to the rendered text.

	@component
-->
<script lang="ts">
	import type { Node } from './types';

	interface SidebarProps {
		/**
		 * The currently selected CLV node, or `undefined` when no node is selected.
		 * Its `label` heads the panel and its `docs` is rendered as the body; an
		 * absent or empty `docs` yields a "No documentation" empty state.
		 */
		selected: Node | undefined;
	}

	let { selected }: SidebarProps = $props();
</script>

<aside
	class="flex h-full w-72 shrink-0 flex-col gap-2 border-l border-neutral-200 bg-white p-4 text-sm text-neutral-900 dark:border-neutral-800 dark:bg-neutral-950 dark:text-neutral-100"
	aria-label="Selection details"
>
	{#if selected}
		<h2 class="text-base font-semibold">{selected.label}</h2>
		{#if selected.docs}
			<p class="whitespace-pre-wrap text-neutral-700 dark:text-neutral-300">{selected.docs}</p>
		{:else}
			<p class="italic text-neutral-500 dark:text-neutral-400">No documentation</p>
		{/if}
	{:else}
		<p class="italic text-neutral-500 dark:text-neutral-400">No node selected</p>
	{/if}
</aside>
