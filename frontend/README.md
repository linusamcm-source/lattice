# Lattice Frontend

SvelteKit + Vite + TypeScript (strict) client for the Lattice live code & agent
visualiser. It streams CLV envelopes over a WebSocket into a reactive store and
renders a **lazy hierarchy** SvelteFlow canvas: only top-level (file) nodes load
on connect, and a node's children are fetched and revealed on demand when the
user expands it.

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

## Lazy hierarchy render model (P1-4)

`src/lib/Graph.svelte` renders a **lazy hierarchy** SvelteFlow canvas from the
`nodes` store. On mount it shows only top-level (root, `parentId` null/absent)
nodes; a node's children appear only after the user expands it.

- **Expansion state** is an explicit `Set<string>` of expanded node ids held in
  `Graph.svelte`. It is the render-side **zoom gate**: a node's children are laid
  out only when its id is in the set, so a function's `variable` children stay off
  the canvas — even when already present in the store — until the function is
  drilled into.
- **Expanding** a node adds its id to the set and invokes the `onExpand` prop
  (default: `requestExpand(socket, nodeId)` against the injected `socket`, which
  the index route wires; tests inject a spy). When the `subtree` reply merges the
  children into the store, the next layout pass reveals them.
- **Collapsing** a node removes its id from the set and calls `collapse`, which
  discards the node's transitive descendants from the store (bounding memory) so
  they leave both the store and the canvas.
- **Affordance & layout.** Each node is a custom `hierarchy` node
  (`src/lib/HierarchyNode.svelte`) rendering its label plus an expand/collapse
  button when it has children. `src/lib/layout.ts` (`buildHierarchy`) walks the
  visible slice of the tree in a stable pre-order (ids sorted per level) and
  assigns deterministic positions: one column per depth (roots at `x = 0`, an
  expanded node's children one column right, their children a further column
  right) and one row per visible node.

The canvas mounts only after `onMount`, so SvelteKit prerender/SSR never
instantiates the browser-only SvelteFlow component. The `Graph` component takes a
`socket?: WebSocket` and an `onExpand?: (nodeId: string) => void` prop; the index
route opens the live socket and passes it down.

## Doc tooltip & selection sidebar (P3-3)

Phase 3 surfaces each node's extracted documentation (`Node.docs`).

- **Hover tooltip.** `buildHierarchy` copies a node's `docs` into its layout data,
  and `src/lib/HierarchyNode.svelte` binds it to a `title` attribute on the node's
  content, so hovering any tier (file/function/variable) shows the description —
  present at every zoom level.
- **Selection sidebar.** Each node's label is a select affordance that calls the
  `onSelect` callback threaded through `buildHierarchy`. `Graph.svelte` holds the
  selected node id, looks the node up live from the `nodes` store, and renders
  `src/lib/Sidebar.svelte` alongside the canvas with its `label` and `docs` (or a
  "No documentation" / "No node selected" empty state). Because the lookup is
  reactive, a `node.upsert` that changes the selected node's `docs` updates the
  sidebar immediately — the live "updating the source updates the shown doc" path.
- **Expand vs select.** The expand/collapse button `stopPropagation`s before
  toggling, so expanding a node never also selects it.

## Edge rendering & control/data-flow filter (P4-4)

Phase 4 draws the graph's edges on the canvas (none were drawn before) and lets
the user filter them by flow class.

- **`buildEdges(graphEdges, visibleNodeIds, filter)`** (`src/lib/layout.ts`)
  projects the `edges` store onto SvelteFlow edges. An edge is rendered only when
  **both** its `source` and `target` are in `visibleNodeIds` — the set of node
  ids `buildHierarchy` emitted — so an edge whose endpoint is off-canvas (e.g.
  inside a collapsed parent) is never drawn. This keeps edges in lockstep with the
  lazy node hierarchy: collapsing a parent shrinks the visible set and its edges
  drop out automatically, with no special-casing.
- **Flow classes.** The CLV `kind` is mapped to one of two classes: `calls` is
  **control flow** (sky stroke) and `param_source` / `data_flows_from` are **data
  flow** (amber stroke, also `animated` as a non-colour cue). `contains` — and any
  other kind — is **never drawn**; containment is already shown by the column
  layout. Each edge carries a typed `data: { kind, flowClass }` marker plus a
  semantic `class` (`lattice-edge-control` / `lattice-edge-data`) whose Tailwind
  stroke utility colours the path from theme tokens (no hard-coded colours).
- **Toggles.** `Graph.svelte` renders two independent checkboxes — **Control
  flow** and **Data flow** — over the canvas, both default on. Each drives the
  matching `filter` flag passed to `buildEdges`, so turning one off removes that
  edge class while leaving the other untouched.
- **Edge routing.** `buildHierarchy` declares each node's left/right `handles` and
  an `initialWidth`/`initialHeight` hint so edges route on the first paint instead
  of after the async measurement pass; the real handle bounds take over once a node
  is measured.

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
