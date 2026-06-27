# Lattice Frontend

SvelteKit + Vite + TypeScript (strict) client for the Lattice live code & agent
visualiser. Phase 0 is a scaffold only: a placeholder index route plus the test,
lint, and build toolchain. The typed WebSocket client and the SvelteFlow two-tier
render land in Story P0-8.

## Stack

- **Svelte 5 + SvelteKit** with the static adapter (`@sveltejs/adapter-static`).
- **Vite** build/dev server.
- **TypeScript** in `strict` mode (`svelte-check`).
- **TailwindCSS v4** via `@tailwindcss/vite` (no PostCSS config; utilities are
  auto-detected from the source tree).
- **@xyflow/svelte** (SvelteFlow) for the graph canvas (used from P0-8).
- **shadcn-svelte** UI primitives — configured via `components.json`.
- **Vitest** (jsdom, globals) + **@testing-library/svelte** for unit/component
  tests, with **@vitest/coverage-v8** writing istanbul-format coverage.

## Commands

All commands run from the `frontend/` directory, or from the repo root with
`npm --prefix frontend run <script>`.

| Command            | Description                                                     |
| ------------------ | --------------------------------------------------------------- |
| `npm run dev`      | Start the Vite dev server.                                      |
| `npm run build`    | Production build (output in `build/`).                          |
| `npm run check`    | `svelte-kit sync` then `svelte-check` (typecheck, zero errors). |
| `npm run lint`     | `prettier --check .` over the project.                          |
| `npm run format`   | `prettier --write .` to auto-format.                            |
| `npm test`         | Run the Vitest suite once.                                      |
| `npm run coverage` | Run Vitest with v8 coverage → `coverage/coverage-final.json`.   |

## Notes

- Coverage uses the v8 provider and emits `coverage/coverage-final.json`
  (istanbul-compatible filename) which the sprint coverage gate autodetects.
  A coverage threshold is intentionally deferred until P0-8 ships real source
  to cover.
- Tailwind is wired so the placeholder route's `text-red-500` is emitted into
  the built CSS, confirming the JIT pipeline works end to end.
