// P9-5 RED — MetricsPanel.svelte metrics debug-panel contract + Graph.svelte toggle.
//
// RED until the impl adds ./MetricsPanel.svelte (plus the `MetricsUpdatePayload`
// type, the `metrics` store, and the `showMetrics` toggle in Graph.svelte). Pinned
// props (mirrors RosterPanel.svelte's single-prop shape):
//   interface MetricsPanelProps {
//     metrics: MetricsUpdatePayload | null;   // Graph.svelte passes $metrics
//   }
//
// DOM contract this test pins:
//   - the panel aside carries data-testid="metrics-panel" (the toggle-mount marker);
//   - data-testid="metric-node-count"     — renders payload.nodeCount;
//   - data-testid="metric-edge-count"     — renders payload.edgeCount;
//   - data-testid="metric-memory"         — memoryBytes shown human-readable (a unit,
//                                            never the raw byte count);
//   - data-testid="metric-events-per-sec" — eventsPerSecMilli / 1000;
//   - data-testid="parse-latency"         — the top parse-latency list (filePath + µs);
//   - a "No metrics" placeholder when metrics is null;
//   - re-rendering with new metrics updates the shown values.
//
// Graph.svelte contract: a `showMetrics` $state(false) checkbox in the canvas
// <fieldset> mounts/unmounts <MetricsPanel> (parity with the showAgents pattern).
//
// No `any`: the fixture is a typed CLV `MetricsUpdatePayload` matching the P9-3 wire.

import { describe, it, expect, afterEach, beforeEach } from 'vitest';
import { render, cleanup, fireEvent } from '@testing-library/svelte';
import { tick } from 'svelte';
import MetricsPanel from './MetricsPanel.svelte';
import Graph from './Graph.svelte';
import { graphStore, initialState } from './ws';
import type { FileParseLatency, MetricsUpdatePayload } from './types';

const metricsPayload: MetricsUpdatePayload = {
	sessionId: 'sess-abc123',
	ts: '2026-06-27T10:32:01.500Z',
	nodeCount: 128,
	edgeCount: 342,
	memoryBytes: 1048576,
	eventsPerSecMilli: 4200,
	parseLatency: [{ filePath: 'src/auth/login.rs', durationUs: 812 }]
};

beforeEach(() => {
	graphStore.set(initialState());
});

afterEach(() => {
	cleanup();
});

describe('MetricsPanel.svelte renders the metrics snapshot', () => {
	it('renders nodeCount and edgeCount', () => {
		const screen = render(MetricsPanel, { props: { metrics: metricsPayload } });
		expect(screen.getByTestId('metric-node-count').textContent).toContain('128');
		expect(screen.getByTestId('metric-edge-count').textContent).toContain('342');
	});

	it('renders memoryBytes human-readable (a unit, not the raw byte count)', () => {
		const screen = render(MetricsPanel, { props: { metrics: metricsPayload } });
		const mem = screen.getByTestId('metric-memory').textContent ?? '';
		expect(mem).toMatch(/\bMB\b/i);
		expect(mem).not.toContain('1048576');
	});

	it('derives events/sec as eventsPerSecMilli / 1000', () => {
		const screen = render(MetricsPanel, { props: { metrics: metricsPayload } });
		expect(screen.getByTestId('metric-events-per-sec').textContent).toContain('4.2');
	});

	it('renders the parse-latency list with each file path and duration', () => {
		const screen = render(MetricsPanel, { props: { metrics: metricsPayload } });
		const latency = screen.getByTestId('parse-latency');
		expect(latency.textContent).toContain('src/auth/login.rs');
		expect(latency.textContent).toContain('812');
	});

	it('shows a placeholder when metrics is null', () => {
		const screen = render(MetricsPanel, { props: { metrics: null } });
		expect(screen.getByText(/no metrics/i)).toBeTruthy();
	});

	it('caps the parse-latency list to the top-N slowest rows, in descending order', () => {
		// 15 unsorted rows (> the N=10 cap) with distinct durations, so the test
		// proves the panel both sorts by durationUs desc AND slices to N.
		const durations = [50, 900, 12, 700, 333, 88, 640, 5, 720, 410, 999, 25, 610, 150, 480];
		const parseLatency: FileParseLatency[] = durations.map((durationUs, i) => ({
			filePath: `src/mod/f${i}.rs`,
			durationUs
		}));
		const screen = render(MetricsPanel, {
			props: { metrics: { ...metricsPayload, parseLatency } }
		});

		const listRows = screen.getByTestId('parse-latency').querySelectorAll('li');
		expect(listRows.length).toBe(10);

		const rendered = Array.from(listRows).map((li) =>
			Number(li.querySelectorAll('span')[1].textContent?.replace(/[^\d]/g, ''))
		);
		expect(rendered).toEqual([999, 900, 720, 700, 640, 610, 480, 410, 333, 150]);
	});

	it('shows the "No parses recorded" placeholder when parseLatency is empty', () => {
		const screen = render(MetricsPanel, {
			props: { metrics: { ...metricsPayload, parseLatency: [] } }
		});
		const latency = screen.getByTestId('parse-latency');
		expect(latency.textContent).toMatch(/no parses recorded/i);
	});
});

describe('MetricsPanel.svelte reflects live updates', () => {
	it('updates the rendered counts when new metrics arrive', async () => {
		const screen = render(MetricsPanel, { props: { metrics: metricsPayload } });
		expect(screen.getByTestId('metric-node-count').textContent).toContain('128');

		await screen.rerender({ metrics: { ...metricsPayload, nodeCount: 200, edgeCount: 500 } });
		expect(screen.getByTestId('metric-node-count').textContent).toContain('200');
		expect(screen.getByTestId('metric-edge-count').textContent).toContain('500');
	});
});

describe('Graph.svelte showMetrics toggle mounts/unmounts MetricsPanel', () => {
	it('hides the panel by default, then mounts and unmounts it on toggle', async () => {
		const screen = render(Graph);
		await tick();

		// Default off: the metrics toggle is unchecked and the panel is absent.
		const toggle = screen.getByRole('checkbox', { name: /metrics/i }) as HTMLInputElement;
		expect(toggle.checked).toBe(false);
		expect(screen.queryByTestId('metrics-panel')).toBeNull();

		// Toggle on → the panel mounts.
		await fireEvent.click(toggle);
		expect(screen.getByTestId('metrics-panel')).toBeTruthy();

		// Toggle off → the panel unmounts.
		await fireEvent.click(toggle);
		expect(screen.queryByTestId('metrics-panel')).toBeNull();
	});
});
