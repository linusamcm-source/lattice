// P8-6 RED — RosterPanel.svelte agent-roster component contract.
//
// RED until the impl adds ./RosterPanel.svelte. Pinned props:
//   interface RosterPanelProps {
//     agents: AgentInfo[];                    // the roster (Graph.svelte passes $agents)
//     selectedAgentId?: string;               // currently drilled-in agentId (optional highlight)
//     onSelect?: (agentId: string) => void;   // drill-down: fired with the bare agentId on click
//   }
//
// DOM contract this test pins:
//   - exactly ONE entry per distinct agentId, regardless of how many processIds share it;
//   - each entry is a clickable element with data-testid={`agent-${agentId}`};
//   - each entry carries data-active="true|false" — active iff ANY of that agentId's
//     processes is active;
//   - the entry is colour-coded by the agent's (sanitised) colour;
//   - clicking the entry calls onSelect(agentId);
//   - a malicious colour is sanitised before it reaches a style binding (no raw CSS injection).
//
// No `any`: every fixture is a typed CLV `AgentInfo`.

import { describe, it, expect, afterEach, vi } from 'vitest';
import { render, cleanup, fireEvent } from '@testing-library/svelte';
import RosterPanel from './RosterPanel.svelte';
import type { AgentInfo } from './types';

const agent = (overrides: Partial<AgentInfo>): AgentInfo => ({
	processId: 1,
	agentId: 'tdd-green',
	agentType: 'implementation',
	color: '#2ecc71',
	status: 'active',
	...overrides
});

afterEach(() => {
	cleanup();
});

describe('RosterPanel.svelte per-agentId roster', () => {
	it('collapses two processIds under one agentId into a single entry', () => {
		const procA = agent({ processId: 1, status: 'active' });
		const procB = agent({ processId: 2, status: 'inactive' });
		const screen = render(RosterPanel, { props: { agents: [procA, procB] } });
		expect(screen.getAllByTestId('agent-tdd-green')).toHaveLength(1);
	});

	it('marks an agentId active when ANY of its processes is active', () => {
		const procA = agent({ processId: 1, status: 'active' });
		const procB = agent({ processId: 2, status: 'inactive' });
		const screen = render(RosterPanel, { props: { agents: [procA, procB] } });
		expect(screen.getByTestId('agent-tdd-green').getAttribute('data-active')).toBe('true');
	});

	it('marks an agentId inactive when all of its processes are inactive', () => {
		const dead = agent({
			agentId: 'security-scanner',
			agentType: 'security',
			color: '#e67e22',
			status: 'inactive'
		});
		const screen = render(RosterPanel, { props: { agents: [dead] } });
		expect(screen.getByTestId('agent-security-scanner').getAttribute('data-active')).toBe('false');
	});

	it('renders one entry per distinct agentId', () => {
		const green = agent({ agentId: 'tdd-green', processId: 1, status: 'active' });
		const greenDup = agent({ agentId: 'tdd-green', processId: 2, status: 'inactive' });
		const sec = agent({
			agentId: 'security-scanner',
			agentType: 'security',
			color: '#e67e22',
			processId: 3,
			status: 'inactive'
		});
		const screen = render(RosterPanel, { props: { agents: [green, greenDup, sec] } });
		expect(screen.getAllByTestId('agent-tdd-green')).toHaveLength(1);
		expect(screen.getAllByTestId('agent-security-scanner')).toHaveLength(1);
	});

	it('colour-codes the entry with the agent colour', () => {
		const screen = render(RosterPanel, { props: { agents: [agent({ color: '#2ecc71' })] } });
		// The sanitised colour reaches the entry (style var / data attr / inline style).
		expect(screen.getByTestId('agent-tdd-green').outerHTML).toContain('#2ecc71');
	});
});

describe('RosterPanel.svelte dynamic roster updates (P8-6)', () => {
	it('flips an entry active -> inactive when its process goes inactive on re-render', async () => {
		const screen = render(RosterPanel, {
			props: { agents: [agent({ processId: 1, status: 'active' })] }
		});
		expect(screen.getByTestId('agent-tdd-green').getAttribute('data-active')).toBe('true');

		await screen.rerender({ agents: [agent({ processId: 1, status: 'inactive' })] });
		expect(screen.getByTestId('agent-tdd-green').getAttribute('data-active')).toBe('false');
	});

	it('keeps ONE active entry after a respawn (new processId, same agentId)', async () => {
		const screen = render(RosterPanel, {
			props: { agents: [agent({ processId: 1, status: 'inactive' })] }
		});
		expect(screen.getByTestId('agent-tdd-green').getAttribute('data-active')).toBe('false');

		// Respawn: the same agentId reappears under a fresh processId, now active. The
		// dead row may still be reported, but the panel must collapse to ONE entry and
		// show it active (active iff ANY process is active).
		await screen.rerender({
			agents: [
				agent({ processId: 1, status: 'inactive' }),
				agent({ processId: 2, status: 'active' })
			]
		});
		expect(screen.getAllByTestId('agent-tdd-green')).toHaveLength(1);
		expect(screen.getByTestId('agent-tdd-green').getAttribute('data-active')).toBe('true');
	});
});

describe('RosterPanel.svelte drill-down selection (P8-6)', () => {
	it('calls onSelect with the bare agentId when an entry is clicked', async () => {
		const onSelect = vi.fn();
		const screen = render(RosterPanel, {
			props: { agents: [agent({ agentId: 'tdd-green' })], onSelect }
		});
		await fireEvent.click(screen.getByTestId('agent-tdd-green'));
		expect(onSelect).toHaveBeenCalledTimes(1);
		expect(onSelect).toHaveBeenCalledWith('tdd-green');
	});
});

describe('RosterPanel.svelte sanitises agent colour before style binding (P8-6 security)', () => {
	it('never injects a malicious colour into a style attribute', () => {
		const evil = agent({ color: 'red;background:url(x)' });
		const screen = render(RosterPanel, { props: { agents: [evil] } });
		// No element may carry the injected CSS in its style attribute.
		expect(screen.container.querySelector('[style*="url("]')).toBeNull();
		expect(screen.container.querySelector('[style*="background"]')).toBeNull();
		// And the raw payload must not appear anywhere as live markup.
		expect(screen.container.innerHTML).not.toContain('url(x)');
	});
});
