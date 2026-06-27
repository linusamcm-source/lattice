# Lattice Phase 0 — Walking Skeleton

End-to-end vertical slice so the whole pipeline is live before any enrichment: a `notify`
file-watcher feeds a `syn`-based Rust parser, which patches an in-memory graph, which a
`tokio-tungstenite` WebSocket server streams as CLV envelopes to a SvelteKit + SvelteFlow client
that renders a flat two-tier view. System-level "done": editing a Rust file (renaming a function)
updates that function node's label live in the browser.

**Grounding (verified this session):**
- Cargo workspace at repo root (`Cargo.toml`, `resolver = "2"`, member `crates/backend`).
- `crates/backend` has `src/lib.rs` (pub `protocol_sentinel() -> &'static str` → `"#CLV1"`, with a
  passing unit test `tests::sentinel_is_clv1`) and `src/main.rs` (binary `lattice`). No other
  backend modules exist yet.
- `just qg` = `cargo fmt --check` + `cargo clippy --all-targets --all-features -D warnings` +
  `cargo test --all`; currently green.
- `frontend/` does **not** exist yet.
- Wire schema is fixed by `docs/orignal_specs/DATA_MODEL.md` §A.1 (ids: `node = type:path:symbol`,
  `edge = e:source->target`), §A.2 (Node), §A.3 (Edge), §A.4 (Event envelope `{v,ts,sessionId,type,
  payload}`). JSON keys are camelCase (`parentId`, `childIds`).

**Scope discipline (BUILD_PLAN.md Phase 0):** Rust language only (`tree-sitter`/multi-language is
Phase 2). Persistence is deliberately deferred to Phase 7 (in-memory only here, per "walking
skeleton first"). The event types Phase 0 exercises are **`snapshot`, `node.upsert`, `node.remove`,
`edge.upsert`, `edge.remove`** (the earlier "only node.upsert/snapshot" note was wrong — the diff
emits removals too). No test-tracking, agents, doc-extraction, or parameter edges (later phases).

**Render model (Phase 0):** the canvas renders a flat **two-tier** view — every `file` node **and
its direct `function` children** — with **no expand/collapse interaction** (lazy-load + zoom-gating
is Phase 1). Two tiers are required so the headline demo is observable: a function's node is on
screen, so renaming the function updates a visible label. Layout is deterministic (files stacked in
a column; their functions offset to the right), assigning a distinct `{x,y}` to every node —
SvelteFlow requires node positions and the CLV Node carries none.

**Phase 0 wire payload contract (pins the payloads DATA_MODEL §A.4 leaves as `payload:{}`; §A.5 only
specifies test/agent/hot_edge/roster payloads).** Both ends — P0-1 types, P0-3 emits, P0-8 consumes
— bind to exactly this:
- `snapshot`     → `{ "nodes": Node[], "edges": Edge[] }`
- `node.upsert`  → `{ "node": Node }`
- `node.remove`  → `{ "id": string }`
- `edge.upsert`  → `{ "edge": Edge }`
- `edge.remove`  → `{ "id": string }`

  `Node`/`Edge` are the §A.2/§A.3 shapes. This is the single interop source of truth for Phase 0.

**Commands the stories inherit:** backend — `just qg`, `just test` (`cargo test --all`). Frontend —
run from the `frontend/` directory (or `npm --prefix frontend run …`): `npm run build`, `npm run
check` (svelte-check), `npm run lint`, `npm test` (vitest), `npm run dev`. New backend crates are
added to `crates/backend/Cargo.toml` per story (every backend story declares that file in `Touches`
so the scheduler serializes Cargo.toml edits and they never collide).

---

## Story P0-1: Define CLV wire types, payloads, and deterministic id helpers

Add the serde data types for the JSON-over-WebSocket contract in a new module
`crates/backend/src/wire.rs`, exported from `lib.rs`. Mirror `DATA_MODEL.md` §A.2–A.4 exactly:
`Node`, `Edge`, `EventEnvelope { v, ts, sessionId, type, payload }`, and the `NodeType`,
`NodeStatus`, `EdgeKind`, `EventType` enums. Also type the **Phase 0 wire payload contract** above as
the `payload` variants (`snapshot {nodes,edges}`, `node.upsert {node}`, `node.remove {id}`,
`edge.upsert {edge}`, `edge.remove {id}`). Provide id helpers `node_id(NodeType, path, symbol)` and
`edge_id(src_symbol, dst_symbol)`. JSON keys are camelCase via `#[serde(rename_all = "camelCase")]`;
enums serialize to the spec's string forms (`EventType::NodeUpsert` → `"node.upsert"`,
`NodeType::Function` → `"function"`, `NodeType::File` → `"file"`).

### Depends On: none
### Touches: crates/backend/src/wire.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- `node_id(NodeType::Function, "src/auth/login.rs", "authenticate")` returns
  `"fn:src/auth/login.rs:authenticate"`; `node_id(NodeType::File, "src/auth/login.rs", "")` returns
  `"file:src/auth/login.rs"`.
- `edge_id("authenticate", "verify_token")` returns `"e:authenticate->verify_token"`.
- Serializing a `Node` to `serde_json::Value` yields camelCase keys: it has a `"parentId"` key and a
  `"childIds"` key (not snake_case).
- `serde_json::to_string` of an `EventEnvelope` whose `type` is `EventType::NodeUpsert` contains
  `"type":"node.upsert"`; a `NodeType::File` field serializes to `"file"`.
- A `node.upsert` envelope serializes its payload as an object with a single `"node"` key; a
  `snapshot` envelope's payload serializes with `"nodes"` and `"edges"` array keys; a `node.remove`
  payload serializes with a single `"id"` string key (asserts the wire payload contract).
- A `Node` round-trips: `from_str(to_string(node))` equals the original (derive `PartialEq`).

### Definition of Done
- `rust-tester` RED tests first, then `rust-developer` implements; coverage ≥80% on the new module.
- `just qg` green (fmt + clippy -D warnings + tests).
- Every public type/function has a `///` doc comment; `wire.rs` has a `//!` module doc; `lib.rs`
  module-level doc updated to mention the wire module (AGENT_PROTOCOL.md §6 cascade).

## Story P0-2: syn-based Rust file parser → structural nodes (panic-free)

Add `crates/backend/src/parser/mod.rs` exposing `parse_rust_source(path: &str, source: &str) ->
ParsedFile` where `ParsedFile` carries the `Vec<Node>` and `Vec<Edge>` for one file. Add `syn`
(features `full`, `visit`, `extra-traits`) and `proc-macro2` **with the `span-locations` feature** to
`Cargo.toml` — without `span-locations`, `proc_macro2` spans report line/col `0`, so it is required
for real `meta.range`. Extract a `file` node plus one `function` node per free function and per
`impl`/`trait` method, with deterministic ids from P0-1's helpers, `contains` edges from the file
node to each function, and `meta.range` (start/end line/col) from each item's span; set `status` to
`unknown`. The caller supplies an already repo-relative `path` (normalization is P0-6's job).
**Must recover from syntax errors**: when `syn::parse_file` returns `Err`, return a `ParsedFile`
with only the file node, `status: error` — never panic, never `.unwrap()` outside tests.

### Depends On: P0-1
### Touches: crates/backend/src/parser/mod.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- Parsing source `"fn foo() {}\nfn bar() {}"` at path `"src/x.rs"` returns nodes including
  `file:src/x.rs` and exactly two function nodes `fn:src/x.rs:foo` and `fn:src/x.rs:bar`.
- Each function node's `label` equals its name and `parentId` equals `"file:src/x.rs"`; a `contains`
  edge exists from the file node to each function node.
- A method `fn m(&self)` inside `impl S` produces a function node `fn:src/x.rs:m`.
- Parsing malformed source `"fn foo( {"` does **not** panic and returns a single file node with
  `status` == `error` (assert via `std::panic::catch_unwind` returning `Ok`).
- With `span-locations` enabled, a function node's `meta.range.startLine` is > 0 and `endLine` >=
  `startLine`.

### Definition of Done
- RED tests first (table-driven over source→expected-ids), then implementation; coverage ≥80%.
- `just qg` green.
- `//!` module doc on `parser`; `///` docs on every public item; cascade note in `lib.rs` doc.

## Story P0-3: In-memory graph model with diff → patch events

Add `crates/backend/src/graph.rs` with a `Graph` holding nodes and edges indexed by id.
`upsert_node`/`upsert_edge` insert-or-update by id; `snapshot()` returns an `EventEnvelope` of type
`snapshot` with payload `{nodes, edges}`; `apply_parsed(ParsedFile) -> Vec<EventEnvelope>` diffs the
file's previous contribution against the new one and returns `node.upsert`/`edge.upsert` (payload
`{node}`/`{edge}`) for added-or-changed elements and `node.remove`/`edge.remove` (payload `{id}`) for
elements that vanished from that file. Re-applying an identical `ParsedFile` returns an empty vector.

### Depends On: P0-1
### Touches: crates/backend/src/graph.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- After `upsert_node(n)` twice with the same id but a changed `label`, the graph holds exactly one
  node for that id and its `label` is the latest value.
- `snapshot()` returns an envelope with `type` == `EventType::Snapshot` whose payload `.nodes`
  contains every current node and `.edges` every current edge.
- `apply_parsed` for a file that newly adds `fn:src/x.rs:foo` returns a `node.upsert` event whose
  payload `.node.id` equals `fn:src/x.rs:foo`.
- When a function present in the previous parse of a file is absent in the new parse, `apply_parsed`
  returns a `node.remove` event whose payload `.id` equals that function's id.
- Applying the same `ParsedFile` twice in a row: the second call returns an empty `Vec`.

### Definition of Done
- RED tests first, then implementation; coverage ≥80%.
- `just qg` green.
- Module/item docs per AGENT_PROTOCOL.md §6; `lib.rs` doc updated.

## Story P0-4: notify file-watcher with quantified debounce, Rust-file filter

Add `crates/backend/src/watcher.rs` exposing an async `watch(root: PathBuf, tx:
tokio::sync::mpsc::Sender<PathBuf>)` built on `notify` that emits the changed path for `.rs` files
only, debounced over a **named `DEBOUNCE: Duration` constant of 150 ms** so a burst of rapid events
for the same path within that window coalesces into one. Non-`.rs` changes are dropped. Never panic
on a watch error — log and continue.

### Depends On: P0-1
### Touches: crates/backend/src/watcher.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- Watching a `tempfile::tempdir()` and creating/writing `a.rs` causes exactly one path to be
  received on the channel within `DEBOUNCE + 1s`, and that path ends with `a.rs`.
- Writing a non-Rust file `notes.txt` yields **no** message within `DEBOUNCE + 1s`.
- Three writes to the same `a.rs` inside `DEBOUNCE` yield at most one received path (assert
  `received <= 1` after draining for `DEBOUNCE * 3`).

### Definition of Done
- RED tests first (tokio async tests with `tempfile`), then implementation; coverage ≥80% on the
  filter + debounce logic.
- `just qg` green.
- Module/item docs per AGENT_PROTOCOL.md §6.

## Story P0-5: WebSocket server streaming CLV envelopes

Add `crates/backend/src/ws.rs` with a `tokio-tungstenite` server. `serve(addr, graph, rx) ->
BoundServer` binds the given address (tests pass `127.0.0.1:0` for an ephemeral port) and **exposes
the bound `SocketAddr`** plus a shutdown handle. On each client connection it first sends the current
graph `snapshot`, then forwards every broadcast `EventEnvelope` to the client as JSON text. A client
text message `{"type":"snapshot"}` triggers a fresh snapshot reply. Fan-out uses a
`tokio::sync::broadcast` channel shared with the graph.

### Depends On: P0-1, P0-3
### Touches: crates/backend/src/ws.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- A test client connecting to the server's bound `SocketAddr` receives, as its first message, a JSON
  envelope whose `type` parses as `snapshot`.
- After a `node.upsert` envelope is published on the broadcast channel, the connected test client
  receives a message whose `type` is `node.upsert` and whose payload `.node.id` matches the published
  one.
- Sending `{"type":"snapshot"}` from the client yields another `snapshot` envelope in reply.

### Definition of Done
- RED integration tests first (`tokio::test`, loopback TCP via the bound `SocketAddr`,
  `tokio-tungstenite` client), then implementation; coverage ≥80% on the message-handling logic.
- `just qg` green.
- Module/item docs per AGENT_PROTOCOL.md §6.

## Story P0-6: Wire watcher → parser → graph → WS in the binary

Wire the components in `crates/backend/src/main.rs` (+ a `run` entry in `lib.rs` for testing): take
a repo path from CLI arg (default `.`); do an initial parse of all `.rs` files into the graph;
**normalize every file path to repo-relative** (strip the supplied repo root) before building node
ids so ids match DATA_MODEL §A.1; start the WS server; and on each debounced watcher event re-parse
that file, `apply_parsed` it, and broadcast the resulting events. `run` binds an **ephemeral port**,
returns the bound `SocketAddr` and a shutdown handle so a test can connect, assert, and tear down
deterministically. This story realizes the phase-level acceptance criterion.

### Depends On: P0-2, P0-3, P0-4, P0-5
### Touches: crates/backend/src/main.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- A `run` integration test pointed at a tempdir containing `a.rs` with `fn alpha() {}`: a connected
  WS client's initial `snapshot` payload `.nodes` contains a node with the **repo-relative** id
  `fn:a.rs:alpha` (not an absolute tempdir path).
- While connected, rewriting the file to `fn beta() {}` causes the client to receive, within
  `DEBOUNCE + 2s`, a `node.upsert` whose payload `.node.id` is `fn:a.rs:beta` and a `node.remove`
  whose payload `.id` is `fn:a.rs:alpha`.
- Pointing `run` at a directory with a syntactically broken `.rs` file still starts and serves a
  snapshot in which the broken file's node has `status: error` (end-to-end panic-free recovery).

### Definition of Done
- RED integration test first, then wiring; coverage ≥80% on the `run` orchestration.
- `just qg` green; `cargo run -- <dir>` starts and logs the bound listen address.
- `lib.rs`/`main.rs` docs updated per AGENT_PROTOCOL.md §6.

## Story P0-7: Scaffold the SvelteKit frontend toolchain

Create `frontend/` as a SvelteKit + Vite + TypeScript (strict) project with TailwindCSS,
`@xyflow/svelte`, `vitest`, and the **frontend test/coverage toolchain P0-8 needs**:
`@testing-library/svelte`, a DOM environment (`jsdom`), and `@vitest/coverage-v8`. Configure vitest
with `environment: 'jsdom'` and a v8 coverage provider that writes `coverage/coverage-final.json`
(istanbul-format, which the sprint coverage gate autodetects) with an 80% threshold. Initialize
shadcn-svelte. The placeholder index route renders an element using the Tailwind class `text-red-500`
so JIT emits it. Establish commands `npm run build`, `npm run check`, `npm run lint`, `npm test`,
`npm run dev` (run from `frontend/`). Include one trivial passing vitest test.

### Depends On: none
### Touches: frontend/**

### Acceptance Criteria
- `npm --prefix frontend run build` exits 0 and produces a build output directory.
- `npm --prefix frontend run check` (svelte-check) exits 0 with no type errors.
- `npm --prefix frontend test` runs vitest and a trivial test (`expect(1+1).toBe(2)`) passes; running
  with coverage produces `frontend/coverage/coverage-final.json`.
- `frontend/package.json` lists `@xyflow/svelte`, `tailwindcss`, `vitest`, `@testing-library/svelte`,
  `jsdom`, and `@vitest/coverage-v8`; `frontend/components.json` (shadcn-svelte) exists.
- The built CSS output contains the `text-red-500` utility (the placeholder route uses it, so JIT
  emits it).

### Definition of Done
- Scaffolded by `typescript-developer`; trivial test by `typescript-tester`.
- `npm run check`, `npm run lint`, `npm test`, `npm run build` all green (from `frontend/`).
- `frontend/README.md` documents the dev/build/test commands and that they run from `frontend/`.

## Story P0-8: WS client + SvelteFlow two-tier live render

Add a typed WebSocket client (`frontend/src/lib/ws.ts`) and Svelte stores that consume the CLV
envelope per the **Phase 0 wire payload contract**: ingest `snapshot {nodes,edges}` into a `nodes`
store and apply `node.upsert {node}` / `node.remove {id}` deltas. A SvelteFlow canvas component
(`frontend/src/lib/Graph.svelte`, used by the index route) renders a flat **two-tier** view — every
`file` node **and its direct `function` children** — assigning a deterministic `{x,y}` to each
(files in a column, functions offset right) so SvelteFlow can place them. No expand/collapse (Phase
1). TypeScript types for `Node`/`Edge`/`EventEnvelope`/payloads mirror DATA_MODEL §A.2–A.4 + the
payload contract, with no `any` at the WS boundary.

### Depends On: P0-7
### Touches: frontend/src/**, frontend/tests/**

### Acceptance Criteria
- Given a fixture `snapshot` envelope with a file node and one function child, the WS message handler
  populates the `nodes` store with length 2 and includes both ids (vitest unit test).
- Applying a `node.upsert` envelope whose `payload.node` has the same `id` as an existing function
  node but `label` changed `"alpha"`→`"beta"` updates that node's `label` to `"beta"` in the store.
- Applying a `node.remove` envelope removes the node whose id equals `payload.id` from the store.
- A `@testing-library/svelte` test mounting `Graph.svelte` with a store of one `file` node and its
  `function` child (`label: "alpha"`, non-null `parentId`) renders elements containing both the file
  label and `"alpha"`; after a `node.upsert` renaming the function to `"beta"`, the rendered text
  updates to `"beta"`.

### Definition of Done
- RED vitest/component tests first (`typescript-tester`), then implementation (`typescript-developer`);
  coverage ≥80% on `ws.ts` + store logic.
- `npm run check`, `npm run lint`, `npm test` green (from `frontend/`).
- TSDoc on exported functions and the store contract; `frontend/README.md` notes the WS event flow
  and the two-tier render model.
