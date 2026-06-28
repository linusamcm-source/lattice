# Lattice Frontend

SvelteKit + Vite + TypeScript (strict) client for the Lattice live code & agent
visualiser. The Phase 0 walking skeleton streams CLV envelopes over a WebSocket
into a reactive store and renders a flat two-tier SvelteFlow canvas (file nodes
and their direct function children).

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

## WebSocket event flow (P0-8)

The CLV wire contract is mirrored, `any`-free, in `src/lib/types.ts`. The
`EventEnvelope` type is a discriminated union over `type`, so the five Phase 0
payloads (`snapshot`, `node.upsert`, `node.remove`, `edge.upsert`, `edge.remove`)
narrow without casts. `src/lib/ws.ts` is the client:

1. `connect(url)` opens a `WebSocket` and registers an `onmessage` handler.
2. `parseEnvelope(raw)` validates each untrusted message (JSON-parsing strings,
   rejecting unknown `type`s and non-object payloads) and returns a typed
   `EventEnvelope` or `null` — it never throws and never widens to `any`.
3. `applyEvent(state, envelope)` is a **pure reducer** that folds one envelope
   into an immutable `GraphState` (`Map<string, Node>` / `Map<string, Edge>`),
   so it is unit-testable without a live socket. `snapshot` replaces the whole
   graph; `*.upsert` inserts-or-updates by id; `*.remove` deletes by id.
4. `ingest(envelope)` applies the reducer to the `graphStore`; components consume
   the derived `nodes` / `edges` stores. Auto-reconnect is deferred to Phase 9.

## Lazy expand / collapse flow (P1-3)

Phase 1 makes the client lazy: the initial `snapshot` carries only top-level
(file) nodes, and a node's children are fetched on demand.

- **Expand.** `requestExpand(socket, nodeId)` sends the frame
  `{"type":"expand","nodeId":"<id>"}` (mirroring the `{"type":"snapshot"}`
  resync request). The backend replies with a `subtree` envelope —
  `payload { parentId, nodes, edges }` — carrying that node's **direct**
  children and the `contains` edges to them. `applyEvent` handles `subtree` by
  **merging** those nodes/edges into the store by id (existing entries
  preserved); it is not a whole-graph replacement. Children are never
  pre-fetched — only this explicit request loads them.
- **Collapse.** `collapse(state, nodeId)` is a pure reducer that returns a new
  `GraphState` with `nodeId`'s **transitive** descendants (everything reachable
  by following `parentId` down from `nodeId`) discarded, along with any edge
  whose endpoint was removed. `nodeId` itself and unrelated nodes are kept. This
  bounds client memory as the user drills in and out of a deep tree.

Reconnect-resync of expanded subtrees is deferred to Phase 9.

## Two-tier render model (P0-8)

`src/lib/Graph.svelte` renders a flat **two-tier** SvelteFlow canvas from the
`nodes` store. The CLV `Node` carries no coordinates, so `src/lib/layout.ts`
(`buildTwoTier`) assigns deterministic positions: every `file` node stacks in a
left column (`x = 0`), and each file's direct `function` children (nodes whose
`parentId` equals a present file id) sit in a right column (`x = 280`), stepped
so they never overlap. Labels render through SvelteFlow's default node
(`data.label`). There is **no expand/collapse** — lazy-load and zoom-gating are
Phase 1. The canvas mounts only after `onMount`, so SvelteKit prerender/SSR never
instantiates the browser-only SvelteFlow component.

## Notes

- Coverage uses the v8 provider and emits `coverage/coverage-final.json`
  (istanbul-compatible filename) which the sprint coverage gate autodetects.
  `ws.ts` and its reducer are covered ≥80% (currently 100% lines).
- `vite.config.ts` registers `@testing-library/svelte/vite`'s `svelteTesting()`
  plugin (adds the `browser` resolve condition under Vitest so Svelte 5's client
  build is used) and `src/test-setup.ts`, which stubs the `ResizeObserver`,
  `DOMMatrixReadOnly`, and `matchMedia` globals `@xyflow/svelte` needs in jsdom.
- Tailwind is wired so the index route's `text-red-500` is emitted into the
  built CSS, confirming the JIT pipeline works end to end.
