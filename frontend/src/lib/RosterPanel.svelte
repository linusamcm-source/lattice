<!--
	RosterPanel.svelte — P8-6 live agent roster panel.

	Mirrors `Sidebar.svelte`'s fixed-width `aside`, listing the Phase-8 agent layer
	beside the selection sidebar. The CLV `agent.roster` may carry several rows for
	one agent (one per OS `processId`), so the panel collapses them to **one entry
	per distinct `agentId`**: an agent shows as active iff **any** of its processes
	is active. Each entry is a clickable button carrying `data-testid="agent-<id>"`
	and `data-active="true|false"`; clicking it fires `onSelect(agentId)` with the
	**bare** agentId, driving `Graph.svelte`'s agent → code drill-down highlight.

	Accessibility: liveness is **not** colour-only. Each entry's accessible name folds
	in the agentId *and* its `active`/`inactive` state (`aria-label`), and the status
	indicator is a shape cue (a filled dot when active, a hollow ring when inactive)
	so the distinction survives greyscale and meets WCAG 1.4.11 non-text contrast. A
	polite `role="status"` live region announces when an agent enters/leaves the
	active set and reports the agent → code drill-down highlight count, so status
	flips and selections reach screen readers.

	Security: an agent's `color` arrives over the wire and is **never** bound into a
	`style` raw — it is passed through `safeColor` first (closing the deferred P8-5
	XSS). A valid `#hex`/`rgb()` is exposed as the `--agent-color` custom property
	on the entry; a rejected (null) colour sets no style and the entry falls back to
	a neutral token via the `var(...)` fallback.

	@component
-->
<script lang="ts">
	import type { AgentInfo } from './types';
	import { safeColor } from './layout';

	interface RosterPanelProps {
		/** The live roster (one row per process); `Graph.svelte` passes `$agents`. */
		agents: AgentInfo[];
		/** The currently drilled-in agentId, highlighted when present. */
		selectedAgentId?: string;
		/**
		 * Number of code nodes the drilled-in agent authored (the agent → code
		 * highlight size). Announced via the live region so the highlight reaches
		 * assistive tech; absent/undefined when nothing is drilled into.
		 */
		authoredCount?: number;
		/** Drill-down callback fired with the bare agentId when an entry is clicked. */
		onSelect?: (agentId: string) => void;
	}

	let { agents, selectedAgentId, authoredCount, onSelect }: RosterPanelProps = $props();

	/** One collapsed roster row: a distinct agentId, its colour, and liveness. */
	type RosterEntry = { agentId: string; color: string; active: boolean };

	// Collapse the per-process roster into one entry per distinct agentId,
	// preserving first-seen order; an agentId is active iff any process is active.
	const roster = $derived.by<RosterEntry[]>(() => {
		const byId = new Map<string, RosterEntry>();
		for (const a of agents) {
			const existing = byId.get(a.agentId);
			if (existing) existing.active ||= a.status === 'active';
			else
				byId.set(a.agentId, { agentId: a.agentId, color: a.color, active: a.status === 'active' });
		}
		return [...byId.values()];
	});

	// Polite live region: announce liveness flips and drill-down highlights to AT.
	let liveMessage = $state('');

	// Announce agents entering/leaving the active set. The first run only records
	// the baseline (the initial roster is not announced); later flips are spoken.
	let prevActive = new Set<string>();
	let trackedActive = false;
	$effect(() => {
		const current = new Set(roster.filter((e) => e.active).map((e) => e.agentId));
		if (trackedActive) {
			const parts = [
				...[...current].filter((id) => !prevActive.has(id)).map((id) => `${id} active`),
				...[...prevActive].filter((id) => !current.has(id)).map((id) => `${id} inactive`)
			];
			if (parts.length > 0) liveMessage = parts.join(', ');
		}
		prevActive = current;
		trackedActive = true;
	});

	// Announce the agent → code drill-down highlight whenever the selection or its
	// authored-node count changes; the first run only records the baseline.
	let trackedDrill = false;
	$effect(() => {
		const id = selectedAgentId;
		const count = authoredCount ?? 0;
		if (trackedDrill && id !== undefined) {
			liveMessage = `Highlighting ${count} node${count === 1 ? '' : 's'} authored by ${id}`;
		}
		trackedDrill = true;
	});
</script>

<aside
	class="flex h-full w-72 shrink-0 flex-col gap-2 border-l border-neutral-200 bg-white p-4 text-sm text-neutral-900 dark:border-neutral-800 dark:bg-neutral-950 dark:text-neutral-100"
	aria-label="Agent roster"
>
	<h2 class="text-base font-semibold">Agents</h2>
	<p class="sr-only" role="status" aria-live="polite">{liveMessage}</p>
	{#if roster.length === 0}
		<p class="italic text-neutral-500 dark:text-neutral-400">No agents</p>
	{:else}
		<ul class="flex flex-col gap-1">
			{#each roster as entry (entry.agentId)}
				{@const safe = safeColor(entry.color)}
				<li>
					<button
						type="button"
						data-testid={`agent-${entry.agentId}`}
						data-active={entry.active ? 'true' : 'false'}
						aria-label={`${entry.agentId}, ${entry.active ? 'active' : 'inactive'}`}
						aria-current={entry.agentId === selectedAgentId ? 'true' : undefined}
						style={safe ? `--agent-color: ${safe};` : undefined}
						class={`flex w-full items-center gap-2 rounded-md border px-2 py-1.5 text-left ${
							entry.agentId === selectedAgentId
								? 'border-violet-500 bg-violet-50 dark:border-violet-400 dark:bg-violet-950'
								: 'border-neutral-200 hover:bg-neutral-100 dark:border-neutral-800 dark:hover:bg-neutral-900'
						}`}
						onclick={() => onSelect?.(entry.agentId)}
					>
						<span
							class="h-2.5 w-2.5 shrink-0 rounded-full bg-[var(--agent-color,var(--color-neutral-400))]"
							aria-hidden="true"
						></span>
						<span class="flex-1 truncate">{entry.agentId}</span>
						<!--
							Liveness as a SHAPE cue (not colour-only): active = a filled dot,
							inactive = a hollow ring. Border contrast meets WCAG 1.4.11 (3:1) and
							the fill/outline difference survives greyscale. The state is also in
							the button's `aria-label`, so it is `aria-hidden` here.
						-->
						<span
							class={`h-2 w-2 shrink-0 rounded-full border-2 ${
								entry.active
									? 'border-green-700 bg-green-700 dark:border-green-400 dark:bg-green-400'
									: 'border-neutral-500 bg-transparent dark:border-neutral-400'
							}`}
							aria-hidden="true"
						></span>
					</button>
				</li>
			{/each}
		</ul>
	{/if}
</aside>
