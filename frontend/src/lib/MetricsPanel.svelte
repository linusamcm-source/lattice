<!--
	MetricsPanel.svelte — P9-5 self-observability debug panel.

	Mirrors `RosterPanel.svelte`'s fixed-width accessible `aside`, rendering the
	latest Phase-9 `metrics.update` snapshot beside the roster/selection sidebar:
	live node/edge counts, a human-readable memory estimate, broadcast throughput
	(events/sec, derived from the integer `eventsPerSecMilli` ×1000 wire field), and
	the *slowest* per-file parse latencies, capped to the top N (see
	`MAX_PARSE_LATENCY_ROWS`). `Graph.svelte` passes `$metrics`, so the panel
	repaints whenever the emitter broadcasts a fresh snapshot.

	Accessibility: the panel is a polite `role="status"` live region so counts and
	latency reaching screen readers as they update. Before the first snapshot arrives
	`metrics` is `null` and the panel shows a "No metrics" placeholder rather than
	empty fields. No colour-only information and no hard-coded colours — layout uses
	Tailwind tokens, matching `RosterPanel`.

	@component
-->
<script lang="ts">
	import type { MetricsUpdatePayload } from './types';

	interface MetricsPanelProps {
		/**
		 * The latest self-observability snapshot, or `null` before the first
		 * `metrics.update` arrives. `Graph.svelte` passes the derived `$metrics`.
		 */
		metrics: MetricsUpdatePayload | null;
	}

	let { metrics }: MetricsPanelProps = $props();

	/**
	 * Render a raw byte count as a human-readable size (e.g. `1048576` → `1.0 MB`),
	 * so the panel never surfaces an unreadable raw byte figure. Scales by 1024 up
	 * to TB and keeps one decimal place.
	 */
	function formatBytes(bytes: number): string {
		const units = ['B', 'KB', 'MB', 'GB', 'TB'];
		let value = bytes;
		let unit = 0;
		while (value >= 1024 && unit < units.length - 1) {
			value /= 1024;
			unit += 1;
		}
		return `${value.toFixed(1)} ${units[unit]}`;
	}

	// Broadcast throughput in events/sec, from the integer `eventsPerSecMilli`
	// (events/sec ×1000) wire field — kept integer on the wire to preserve the
	// backend `Eq` derive, divided back to a one-decimal rate for display.
	const eventsPerSec = $derived(metrics ? (metrics.eventsPerSecMilli / 1000).toFixed(1) : '0');

	/** Upper bound on how many parse-latency rows the panel renders. */
	const MAX_PARSE_LATENCY_ROWS = 10;

	/**
	 * The parse-latency rows to render: `parseLatency` sorted by `durationUs`
	 * descending and sliced to the slowest {@link MAX_PARSE_LATENCY_ROWS}. The story
	 * asks for the *top* parse latencies, and rendering the whole (potentially large
	 * or hostile) `parseLatency` array unbounded would put needless resource pressure
	 * on the client — so the view is capped. Copies before sorting to avoid mutating
	 * the store's snapshot.
	 */
	const topParseLatency = $derived(
		metrics
			? [...metrics.parseLatency]
					.sort((a, b) => b.durationUs - a.durationUs)
					.slice(0, MAX_PARSE_LATENCY_ROWS)
			: []
	);
</script>

<aside
	class="flex h-full w-72 shrink-0 flex-col gap-2 border-l border-neutral-200 bg-white p-4 text-sm text-neutral-900 dark:border-neutral-800 dark:bg-neutral-950 dark:text-neutral-100"
	aria-label="Metrics"
	role="status"
	aria-live="polite"
	data-testid="metrics-panel"
>
	<h2 class="text-base font-semibold">Metrics</h2>
	{#if metrics === null}
		<p class="italic text-neutral-500 dark:text-neutral-400">No metrics</p>
	{:else}
		<dl class="flex flex-col gap-1">
			<div class="flex items-center justify-between gap-2">
				<dt class="text-neutral-500 dark:text-neutral-400">Nodes</dt>
				<dd data-testid="metric-node-count" class="font-mono">{metrics.nodeCount}</dd>
			</div>
			<div class="flex items-center justify-between gap-2">
				<dt class="text-neutral-500 dark:text-neutral-400">Edges</dt>
				<dd data-testid="metric-edge-count" class="font-mono">{metrics.edgeCount}</dd>
			</div>
			<div class="flex items-center justify-between gap-2">
				<dt class="text-neutral-500 dark:text-neutral-400">Memory</dt>
				<dd data-testid="metric-memory" class="font-mono">{formatBytes(metrics.memoryBytes)}</dd>
			</div>
			<div class="flex items-center justify-between gap-2">
				<dt class="text-neutral-500 dark:text-neutral-400">Events/sec</dt>
				<dd data-testid="metric-events-per-sec" class="font-mono">{eventsPerSec}</dd>
			</div>
		</dl>
		<h3 class="mt-2 text-xs font-medium uppercase tracking-wide text-neutral-500">Parse latency</h3>
		<ul data-testid="parse-latency" class="flex flex-col gap-1">
			{#if topParseLatency.length === 0}
				<li class="italic text-neutral-500 dark:text-neutral-400">No parses recorded</li>
			{:else}
				{#each topParseLatency as row (row.filePath)}
					<li class="flex items-center justify-between gap-2">
						<span class="flex-1 truncate">{row.filePath}</span>
						<span class="font-mono text-neutral-500 dark:text-neutral-400">{row.durationUs} µs</span
						>
					</li>
				{/each}
			{/if}
		</ul>
	{/if}
</aside>
