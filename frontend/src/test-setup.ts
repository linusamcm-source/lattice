/**
 * Vitest jsdom setup: register the minimal browser globals that `@xyflow/svelte`
 * (via `@xyflow/system`) reaches for but jsdom does not implement. Referenced from
 * `vite.config.ts` via `test.setupFiles`.
 */

import { vi } from 'vitest';

class ResizeObserverMock {
	observe(): void {}
	unobserve(): void {}
	disconnect(): void {}
}

// `@xyflow/system` reads `m22` (the zoom factor) off `new window.DOMMatrixReadOnly(transform)`.
class DOMMatrixReadOnlyMock {
	m22 = 1;
	constructor(_transform?: string) {}
}

vi.stubGlobal('ResizeObserver', ResizeObserverMock);
vi.stubGlobal('DOMMatrixReadOnly', DOMMatrixReadOnlyMock);

if (typeof window !== 'undefined' && !window.matchMedia) {
	window.matchMedia = ((query: string) => ({
		matches: false,
		media: query,
		onchange: null,
		addListener: () => {},
		removeListener: () => {},
		addEventListener: () => {},
		removeEventListener: () => {},
		dispatchEvent: () => false
	})) as unknown as typeof window.matchMedia;
}
