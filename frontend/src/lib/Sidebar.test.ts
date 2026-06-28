import { describe, it, expect, afterEach } from 'vitest';
import { render, cleanup } from '@testing-library/svelte';
import Sidebar from './Sidebar.svelte';
import type { Node } from './types';

const baseNode = (overrides: Partial<Node>): Node => ({
	id: 'fn:src/x.rs:add',
	type: 'function',
	label: 'add',
	parentId: null,
	childIds: [],
	status: 'unknown',
	...overrides
});

afterEach(() => {
	cleanup();
});

describe('Sidebar.svelte selection panel', () => {
	it('renders the selected node label and its docs', () => {
		const selected = baseNode({ label: 'add', docs: 'Adds two numbers.' });
		const screen = render(Sidebar, { props: { selected } });
		expect(screen.getByText('add')).toBeTruthy();
		expect(screen.getByText('Adds two numbers.')).toBeTruthy();
	});

	it('renders a no-documentation indicator when docs is undefined (never the literal undefined)', () => {
		const selected = baseNode({ label: 'bare', docs: undefined });
		const screen = render(Sidebar, { props: { selected } });
		expect(screen.getByText('bare')).toBeTruthy();
		expect(screen.getByText(/no documentation/i)).toBeTruthy();
		expect(screen.queryByText('undefined')).toBeNull();
	});

	it('renders the no-documentation indicator when docs is the empty string', () => {
		const selected = baseNode({ label: 'empty', docs: '' });
		const screen = render(Sidebar, { props: { selected } });
		expect(screen.getByText(/no documentation/i)).toBeTruthy();
	});

	it('renders an empty-selection hint when no node is selected', () => {
		const screen = render(Sidebar, { props: { selected: undefined } });
		expect(screen.getByText(/no node selected/i)).toBeTruthy();
	});
});
