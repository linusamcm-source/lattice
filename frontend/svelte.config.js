import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
const config = {
	preprocess: vitePreprocess(),
	kit: {
		// SPA fallback: emit `build/index.html` so the `lattice` binary (Phase 10) can
		// serve it as the catch-all for client-side routes it doesn't have an asset for.
		adapter: adapter({ fallback: 'index.html' })
	}
};

export default config;
