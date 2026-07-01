<!--
	Graph.expanded.harness.svelte — test-only fixture that owns the `$bindable`
	`expanded` open-node set the way `+page.svelte` does, `bind:`s it into
	`Graph.svelte`, and reports its current value out via the `report` callback
	whenever it changes. This lets a component test drive the REAL `toggle()`
	collapse path (expand → expand → collapse) and read back the resulting open set
	to assert the P9-6 stale-descendant fix (a nested collapse prunes the collapsed
	id AND its orphaned descendants). Not shipped in the app.

	@component
-->
<script lang="ts">
	import Graph from './Graph.svelte';

	interface HarnessProps {
		/** Forwarded to Graph as its expand handler (usually a spy). */
		onExpand?: (nodeId: string) => void;
		/** Called with the current `expanded` set on mount and on every change. */
		report: (open: Set<string>) => void;
	}

	let { onExpand, report }: HarnessProps = $props();
	let expanded = $state(new Set<string>());

	// Push the live open set out to the test on every toggle-driven reassignment.
	$effect(() => {
		report(expanded);
	});
</script>

<Graph bind:expanded {onExpand} />
