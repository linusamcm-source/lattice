# Lattice Phase 1 — Hierarchy + Lazy Loading (+ variable zoom-gating)

Turns the Phase-0 "send the whole graph, render two flat tiers" model into a true lazy hierarchy:
the client gets only the **top level** (file nodes) on connect; expanding a node sends an `expand`
request and the backend replies with just that node's direct children; collapsing discards the
descendants to bound client memory. A third tier — **variable** nodes — is added to the parser and
is **zoom-gated**: a function renders as a single node until the user drills into it. System-level
"done" (BUILD_PLAN.md Phase 1): drilling into a function reveals its variables on demand; collapsing
frees them; the canvas stays responsive with a deep tree.

**Grounding (read this session; Phase 0 merged at `56473f6`):**
- `crates/backend/src/parser/mod.rs` — `parse_rust_source(path,source) -> ParsedFile { nodes, edges }`
  currently emits a `file` node + one `function` node per free fn / impl-method, plus `contains`
  edges, panic-free on syntax error (returns a single `status:error` file node). No variable nodes.
- `crates/backend/src/graph.rs` — `Graph` with `upsert_node`/`upsert_edge`, `snapshot() ->
  EventEnvelope` (returns the **whole** graph today), `apply_parsed(ParsedFile) -> Vec<EventEnvelope>`.
- `crates/backend/src/ws.rs` — `serve(addr, Arc<Mutex<Graph>>, broadcast::Sender<EventEnvelope>)`;
  `handle_connection` sends the `snapshot` on connect and replies to a client text frame
  `{"type":"snapshot"}` (matched by `is_snapshot_request`) with a fresh snapshot.
- `crates/backend/src/app.rs` — `run()` wires watcher→parser→graph→ws; its e2e tests assert the
  initial snapshot contains `fn:a.rs:alpha` (a function) and that renaming a fn broadcasts
  `node.upsert(beta)`+`node.remove(alpha)`.
- `crates/backend/src/wire.rs` — `Node` has `parentId`/`childIds`; `NodeType::Variable`, `EventType`,
  and the untagged `Payload` enum exist; ids via `node_id(NodeType, path, symbol)`.
- `frontend/src/lib/ws.ts` — `applyEvent(state, envelope)` (pure reducer; `snapshot` replaces whole,
  `node.upsert`/`node.remove`/`edge.*` deltas), `parseEnvelope` (validates against
  `KNOWN_EVENT_TYPES = {snapshot,node.upsert,node.remove,edge.upsert,edge.remove}`), `graphStore`,
  derived `nodes`/`edges`, `ingest`, `connect(url)`. `Graph.svelte` renders a flat two-tier view of
  the whole store; `layout.ts` has `buildTwoTier`.

**Phase-1 `expand` wire contract (PINNED — deliberately extends DATA_MODEL §A.4, which lists `expand`
as a client→server request but leaves the reply shape open):**
- Client→server (text frame, mirrors `{"type":"snapshot"}`): `{"type":"expand","nodeId":"<id>"}`.
- Server→client reply: an `EventEnvelope` with `type:"subtree"` and payload
  `{ "parentId": "<id>", "nodes": Node[], "edges": Edge[] }`, where `nodes` are the **direct**
  children of `parentId` and `edges` are the `contains` edges from `parentId` to them. Both ends bind
  to exactly this; `Node`/`Edge` are the §A.2/§A.3 shapes. This is the single new interop contract.

**Scope discipline (BUILD_PLAN.md Phase 1):** hierarchy + lazy load + variable zoom-gating only. No
multi-language (Phase 2), doc extraction (Phase 3), param/data-flow edges (Phase 4), test-tracking
(Phase 5), persistence (Phase 7). The live broadcast path (`apply_parsed` → `node.*`/`edge.*` over
the broadcast channel) is unchanged; only the connect/snapshot model becomes lazy and `expand` is
added.

**Commands:** backend `just qg` / `just test`. Frontend (from `frontend/`, real npm at
`/opt/homebrew/bin/npm` — bare `npm` is aliased to bun): `npm run check`, `npm run lint`, `npm test`,
`npm run build`. Coverage gate 80% new-code (cargo-llvm-cov backend; vitest v8 for ws/store logic).

---

## Story P1-1: Parser extracts function-local variable nodes

Extend `parse_rust_source` in `crates/backend/src/parser/mod.rs` to emit a `variable` node for each
**`let` binding** in a function body, as a child of that function. Variable id is
`node_id(NodeType::Variable, path, "<fn>:<name>")` (e.g. `var:src/x.rs:f:x`), `label` is the binding
name, `parentId` is the owning function node id, `status` is `unknown`, and `meta.range` comes from
the binding's span. A `contains` edge links the function node to each variable node. Functions with
no `let` bindings produce no variable nodes (preserves existing Phase-0 parser tests, which use
empty-body fns). Syntax-error recovery is unchanged (single `status:error` file node, panic-free).

### Depends On: none
### Touches: crates/backend/src/parser/mod.rs

### Acceptance Criteria
- Parsing `"fn f() { let x = 1; let y = 2; }"` at path `src/x.rs` yields, besides `file:src/x.rs` and
  `fn:src/x.rs:f`, exactly two `variable` nodes with ids `var:src/x.rs:f:x` and `var:src/x.rs:f:y`,
  each with `parentId == "fn:src/x.rs:f"` and `label` equal to the binding name.
- A `contains` edge exists from `fn:src/x.rs:f` to each of those variable nodes.
- Parsing `"fn g() {}"` yields the function node `fn:src/x.rs:g` and **no** `variable` nodes.
- Each variable node's `meta.range.startLine` is > 0.
- Parsing malformed `"fn bad( {"` still returns a single `file` node with `status == error` and does
  not panic (Phase-0 recovery preserved).

### Definition of Done
- `rust-tester` RED tests first, then `rust-developer`; new parser code ≥80% line-covered.
- `just qg` green (fmt + clippy -D warnings + all tests, including the unchanged Phase-0 tests).
- `///` docs on any new items; parser `//!` updated to mention variable extraction (AGENT_PROTOCOL §6).

## Story P1-2: Lazy backend — top-level snapshot, children query, and `expand` → `subtree`

Make the backend lazy. In `crates/backend/src/graph.rs`: change `snapshot()` to include **only
root nodes** (`parentId == None`, i.e. files) and the edges among them; add
`children_of(&self, node_id: &str) -> (Vec<Node>, Vec<Edge>)` returning the **direct** children
(nodes whose `parentId == node_id`) and the `contains` edges from `node_id` to them (not
grandchildren). In `crates/backend/src/wire.rs`: add `EventType::Subtree` (serialises `"subtree"`)
and a `Payload::Subtree { parent_id, nodes, edges }` (camelCase `parentId`). In
`crates/backend/src/ws.rs`: handle a client `{"type":"expand","nodeId":...}` text frame by calling
`children_of` and replying with a `subtree` envelope. Update the Phase-0 e2e/ws tests that assumed a
whole-graph snapshot to the lazy model (snapshot carries `file:a.rs`, then `expand` yields the
functions). The live broadcast path (`apply_parsed`) is unchanged.

### Depends On: P1-1
### Touches: crates/backend/src/graph.rs, crates/backend/src/wire.rs, crates/backend/src/ws.rs, crates/backend/src/app.rs

### Acceptance Criteria
- `Graph::snapshot()` payload `.nodes` contains every node with `parentId == None` and contains **no**
  node whose `parentId` is set (assert a known function node is absent from the snapshot).
- `Graph::children_of("file:src/x.rs")` returns the direct child `function` nodes and the `contains`
  edges `file→function`, and does **not** include any `variable` (grandchild) node.
- `Graph::children_of("fn:src/x.rs:f")` returns that function's `variable` child nodes.
- Serialising a `Subtree` envelope yields `"type":"subtree"` and a `"parentId"` key (camelCase) in the
  payload (wire unit test).
- A WS test client connecting receives an initial `snapshot` containing `file:a.rs` but **not**
  `fn:a.rs:alpha`; sending `{"type":"expand","nodeId":"file:a.rs"}` (with the repo-relative file id)
  yields a `subtree` envelope whose `payload.parentId` is that file id and whose `payload.nodes`
  includes `fn:a.rs:alpha`.

### Definition of Done
- RED tests first; new/changed backend code ≥80% line-covered; the Phase-0 tests updated to the lazy
  model still pass (no regressions elsewhere).
- `just qg` green.
- `///`/`//!` docs for the new wire variants, `children_of`, and the expand handler; cascade
  `lib.rs`/`graph.rs`/`ws.rs` module docs (AGENT_PROTOCOL §6).

## Story P1-3: Frontend WS — `expand` request, `subtree` reducer, collapse-discard

Extend `frontend/src/lib/types.ts` and `frontend/src/lib/ws.ts` to the Phase-1 contract. Add the
`subtree` envelope variant (`payload { parentId: string; nodes: Node[]; edges: Edge[] }`) to the
`EventEnvelope` union and add `'subtree'` to `KNOWN_EVENT_TYPES` so `parseEnvelope` accepts it (no
`any` at the boundary). Extend `applyEvent` to **merge** a `subtree` payload's nodes/edges into the
store (existing entries preserved). Add `requestExpand(socket: WebSocket, nodeId: string)` that sends
the `{"type":"expand","nodeId":nodeId}` frame, and a pure `collapse(state, nodeId)` that removes
`nodeId`'s transitive descendants (by `parentId` chain) from the store while keeping `nodeId` and
unrelated nodes (bounded memory).

### Depends On: P1-2
### Touches: frontend/src/lib/ws.ts, frontend/src/lib/types.ts, frontend/src/lib/ws.test.ts

### Acceptance Criteria
- A fixture `subtree` envelope (`type:"subtree"`, valid `parentId`/`nodes`/`edges`) passes
  `parseEnvelope` (returns non-null) and a malformed one (missing `payload`) returns `null`.
- `applyEvent(state, subtreeEnvelope)` merges the payload's nodes into the store keyed by id: a store
  pre-loaded with one file node, after applying a subtree carrying that file's function child, has
  both ids present (existing node preserved, child added).
- `collapse(state, "file:a.rs")` on a state holding `file:a.rs` + `fn:a.rs:alpha` (parent the file) +
  `var:a.rs:alpha:x` (parent the fn) removes the function and the variable (transitive descendants)
  and keeps `file:a.rs`.
- `requestExpand(socket, "file:a.rs")` calls `socket.send` exactly once with the string
  `{"type":"expand","nodeId":"file:a.rs"}` (asserted via a mock socket).

### Definition of Done
- RED vitest tests first (`typescript-tester`), then `typescript-developer`; `ws.ts` new logic ≥80%
  line-covered.
- `npm run check`, `npm run lint`, `npm test` green (from `frontend/`).
- TSDoc on the new exports and the `subtree`/collapse contract; `frontend/README.md` notes the lazy
  expand/collapse flow.

## Story P1-4: Frontend render — expand/collapse interaction + zoom-gated variables

Rework `frontend/src/lib/Graph.svelte` (and `layout.ts`) from the flat two-tier render to a lazy
hierarchy. On mount the canvas shows only top-level (file) nodes from the store. A node carries an
expand affordance; activating it invokes an expand handler (`requestExpand` against the live socket,
injectable for tests) and, once the `subtree` reply has merged the children into the store, renders
them. **Variables are zoom-gated**: a function's `variable` children, even if present in the store,
do not render until that function is expanded. Collapsing a node calls `collapse` and removes its
rendered descendants. Layout assigns deterministic `{x,y}` per visible tier.

### Depends On: P1-3
### Touches: frontend/src/lib/Graph.svelte, frontend/src/lib/layout.ts, frontend/src/lib/Graph.test.ts

### Acceptance Criteria
- A `@testing-library/svelte` test mounting `Graph.svelte` with a store of one `file` node renders the
  file label and renders **no** function or variable node (only the top level is shown on mount).
- Triggering expand on the file node invokes the injected expand handler with the file's id; after a
  `subtree` reply merges the function child into the store, the function node's label renders.
- With a store holding a file + an (unexpanded) function + that function's variable child, the
  variable's label is **not** rendered; after expanding the function, the variable's label renders
  (zoom-gating).
- Triggering collapse on the file node removes the function (and its variable) from the rendered
  canvas and from the store (asserts the labels are gone).

### Definition of Done
- RED component tests first (`typescript-tester`), then `typescript-developer`; new render/layout
  logic ≥80% line-covered.
- `npm run check`, `npm run lint`, `npm test`, `npm run build` green (from `frontend/`).
- TSDoc on the component contract; `frontend/README.md` updated to the lazy hierarchy render model.
