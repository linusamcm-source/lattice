# Lattice Phase 0 — Walking Skeleton

End-to-end vertical slice so the whole pipeline is live before any enrichment: a `notify`
file-watcher feeds a `syn`-based Rust parser, which patches an in-memory graph, which a
`tokio-tungstenite` WebSocket server streams as CLV envelopes to a SvelteKit + SvelteFlow client
that renders **root-level nodes only**. System-level "done": editing a Rust file updates a node's
label live in the browser.

**Grounding (verified this session):**
- Cargo workspace at repo root (`Cargo.toml`, `resolver = "2"`, member `crates/backend`).
- `crates/backend` has `src/lib.rs` (pub `protocol_sentinel() -> &'static str` returning `"#CLV1"`,
  with a passing unit test `tests::sentinel_is_clv1`) and `src/main.rs` (binary `lattice` that
  prints the sentinel). No other backend modules exist yet.
- `just qg` (in `justfile`) = `cargo fmt --check` + `cargo clippy --all-targets --all-features -D
  warnings` + `cargo test --all`; currently green.
- `frontend/` does **not** exist yet (confirmed by `ls`).
- Wire schema is fixed by `docs/orignal_specs/DATA_MODEL.md` §A.2 (Node), §A.3 (Edge), §A.4 (Event
  envelope; `type ∈ node.upsert | node.remove | … | snapshot`). Node ids are `type:path:symbol`,
  edge ids `e:source->target` (§A.1). JSON keys are camelCase (`parentId`, `childIds`).

**Scope discipline (BUILD_PLAN.md Phase 0):** Rust language only (`tree-sitter`/multi-language is
Phase 2). Root-level render only — hierarchy/lazy-load is Phase 1. No test-tracking, agents,
doc-extraction, or parameter edges (later phases). **Persistence is deliberately deferred to Phase
7** (`Storage abstraction`): Phase 0 runs purely in-memory per the "walking skeleton first, then
enrich" principle; no `sqlx`/SQLite is introduced here. Only `node.upsert`/`snapshot` event types
are exercised; the full envelope enum is typed but not all variants are emitted yet.

**Commands the stories inherit:** backend — `just qg`, `just test` (`cargo test --all`). Frontend —
`npm run build`, `npm run check` (svelte-check), `npm run lint`, `npm test` (vitest), `npm run dev`.
New backend crates are added to `crates/backend/Cargo.toml` per story (every backend story declares
that file in `Touches` so the scheduler serializes Cargo.toml edits and they never collide).

---

## Story P0-1: Define CLV wire types and deterministic id helpers

Add the serde data types for the JSON-over-WebSocket contract in a new module
`crates/backend/src/wire.rs`, exported from `lib.rs`. Types mirror `DATA_MODEL.md` §A.2–A.4 exactly:
`Node` (id, type, label, parentId, childIds, status, optional docs/signature/meta), `Edge` (id,
source, target, kind, hot), `EventEnvelope` (v, ts, sessionId, type, payload), and the `NodeType`,
`NodeStatus`, `EdgeKind`, `EventType` enums. Provide id helpers `node_id(NodeType, path, symbol)`
and `edge_id(src_symbol, dst_symbol)`. All JSON keys are camelCase via `#[serde(rename_all =
"camelCase")]`; enums serialize to the spec's lowercase/dotted string forms (e.g. `EventType::NodeUpsert`
→ `"node.upsert"`, `NodeType::Function` → `"function"`).

### Depends On: none
### Touches: crates/backend/src/wire.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- `node_id(NodeType::Function, "src/auth/login.rs", "authenticate")` returns the string
  `"fn:src/auth/login.rs:authenticate"`.
- `edge_id("authenticate", "verify_token")` returns `"e:authenticate->verify_token"`.
- Serializing a `Node` to `serde_json::Value` yields camelCase keys: the value has a `"parentId"`
  key and a `"childIds"` key (not `parent_id`/`child_ids`).
- `serde_json::to_string` of an `EventEnvelope` whose `type` is `EventType::NodeUpsert` contains the
  substring `"type":"node.upsert"`; a `NodeType::File` field serializes to `"file"`.
- A `Node` round-trips: `from_str(to_string(node))` equals the original (derive `PartialEq`).

### Definition of Done
- `rust-tester` RED tests written first, then `rust-developer` implements; coverage ≥80% on the new
  module.
- `just qg` green (fmt + clippy -D warnings + tests).
- Every public type/function has a `///` doc comment; `wire.rs` has a `//!` module doc; `lib.rs`
  module-level doc updated to mention the wire module (AGENT_PROTOCOL.md §6 cascade).

## Story P0-2: syn-based Rust file parser → structural nodes (panic-free)

Add `crates/backend/src/parser/mod.rs` exposing `parse_rust_source(path: &str, source: &str) ->
ParsedFile` where `ParsedFile` carries the `Vec<Node>` and `Vec<Edge>` for one file. Use `syn`
(features `full`, `visit`) to extract a `file` node plus one `function` node per free function and
per `impl`/`trait` method, with deterministic ids from P0-1's helpers and `contains` edges from the
file node to each function. Populate `meta.range` (start/end line/col) from each item's span and set
`status` to `unknown`. **Must recover from syntax errors**: when `syn::parse_file` returns `Err`,
return a `ParsedFile` containing only the file node with `status: error` — never panic, never
propagate a panic.

### Depends On: P0-1
### Touches: crates/backend/src/parser/mod.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- Parsing source `"fn foo() {}\nfn bar() {}"` at path `"src/x.rs"` returns nodes including
  `file:src/x.rs` and exactly two function nodes `fn:src/x.rs:foo` and `fn:src/x.rs:bar`.
- Each function node's `label` equals its name (`"foo"`, `"bar"`) and `parentId` equals
  `"file:src/x.rs"`; a `contains` edge exists from the file node to each function node.
- A method `fn m(&self)` inside `impl S` produces a function node `fn:src/x.rs:m` (method extracted).
- Parsing malformed source `"fn foo( {"` does **not** panic and returns a single file node with
  `status` == `error` (assert via `std::panic::catch_unwind` returning `Ok`, or by `Result`).
- A function node's `meta.range.startLine` is > 0 and `endLine` >= `startLine`.

### Definition of Done
- RED tests first (table-driven over source→expected-ids), then implementation; coverage ≥80%.
- `just qg` green.
- `//!` module doc on `parser`; `///` docs on every public item; cascade note in `lib.rs` doc.

## Story P0-3: In-memory graph model with diff → patch events

Add `crates/backend/src/graph.rs` with a `Graph` holding nodes and edges indexed by id.
`upsert_node`/`upsert_edge` insert-or-update by id; `snapshot()` returns an `EventEnvelope` of type
`snapshot` carrying all nodes+edges; `apply_parsed(ParsedFile) -> Vec<EventEnvelope>` diffs the
file's previous contribution against the new one and returns `node.upsert`/`edge.upsert` events for
added-or-changed elements and `node.remove`/`edge.remove` for elements that vanished from that file.
Re-applying an identical `ParsedFile` returns an empty event vector (idempotent).

### Depends On: P0-1
### Touches: crates/backend/src/graph.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- After `upsert_node(n)` twice with the same id but a changed `label`, the graph holds exactly one
  node for that id and its `label` is the latest value.
- `snapshot()` returns an envelope with `type` == `EventType::Snapshot` whose payload contains every
  current node and edge.
- `apply_parsed` for a file that newly adds `fn:src/x.rs:foo` returns at least one `node.upsert`
  event whose payload node id is `fn:src/x.rs:foo`.
- When a function present in the previous parse of a file is absent in the new parse, `apply_parsed`
  returns a `node.remove` event for that function's id.
- Applying the same `ParsedFile` twice in a row: the second call returns an empty `Vec`.

### Definition of Done
- RED tests first, then implementation; coverage ≥80%.
- `just qg` green.
- Module/item docs per AGENT_PROTOCOL.md §6; `lib.rs` doc updated.

## Story P0-4: notify file-watcher with debounce, Rust-file filter

Add `crates/backend/src/watcher.rs` exposing an async `watch(root: PathBuf, tx:
tokio::sync::mpsc::Sender<PathBuf>)` built on `notify` that emits the changed path for `.rs` files
only, debounced so a burst of rapid events for the same path coalesces into one. Non-`.rs` changes
are dropped. Never panic on a watch error — log and continue.

### Depends On: P0-1
### Touches: crates/backend/src/watcher.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- Watching a `tempfile::tempdir()` and creating/writing `a.rs` causes exactly one path to be
  received on the channel within a 1s test timeout, and that path ends with `a.rs`.
- Writing a non-Rust file `notes.txt` in the watched dir yields **no** message on the channel within
  the timeout.
- Three writes to the same `a.rs` within the debounce window yield at most one received path (assert
  `received <= 1` after draining for the debounce interval + margin).

### Definition of Done
- RED tests first (tokio async tests with `tempfile`), then implementation; coverage ≥80% on
  testable logic (filter + debounce); the raw `notify` callback wiring may be thin.
- `just qg` green.
- Module/item docs per AGENT_PROTOCOL.md §6.

## Story P0-5: WebSocket server streaming CLV envelopes

Add `crates/backend/src/ws.rs` with a `tokio-tungstenite` server that, on each client connection,
first sends the current graph `snapshot`, then forwards every broadcast `EventEnvelope` to the
client as JSON text. A client text message `{"type":"snapshot"}` triggers a fresh snapshot reply.
Uses a `tokio::sync::broadcast` channel shared with the graph for fan-out.

### Depends On: P0-1, P0-3
### Touches: crates/backend/src/ws.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- A test client connecting to the server over an ephemeral port receives, as its first message, a
  JSON envelope whose `type` parses as `snapshot`.
- After a `node.upsert` envelope is published on the broadcast channel, the connected test client
  receives a message whose `type` is `node.upsert` and whose payload node id matches the published
  one.
- Sending `{"type":"snapshot"}` from the client yields another `snapshot` envelope in reply.

### Definition of Done
- RED integration tests first (`tokio::test`, real loopback TCP, `tokio-tungstenite` client), then
  implementation; coverage ≥80% on the message-handling logic.
- `just qg` green.
- Module/item docs per AGENT_PROTOCOL.md §6.

## Story P0-6: Wire watcher → parser → graph → WS in the binary

Wire the components in `crates/backend/src/main.rs` (+ a thin `run` entry in `lib.rs` for testing):
take a repo path from CLI arg (default `.`), do an initial parse of all `.rs` files into the graph,
start the WebSocket server, and on each debounced watcher event re-parse that file, `apply_parsed`
it, and broadcast the resulting events. This is the story that realizes the phase-level acceptance
criterion.

### Depends On: P0-2, P0-3, P0-4, P0-5
### Touches: crates/backend/src/main.rs, crates/backend/src/lib.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- A `run`-style integration test pointed at a tempdir containing `a.rs` with `fn alpha() {}`: a
  connected WS client's initial `snapshot` contains a node with id `fn:<…>a.rs:alpha`.
- While connected, renaming the function to `fn beta() {}` (rewriting the file) causes the client to
  receive, within ~1s, a `node.upsert` for `fn:<…>a.rs:beta` (and a `node.remove` for the old
  `alpha` id).
- Pointing the binary at a directory with a syntactically broken `.rs` file still starts and serves
  a snapshot (the broken file's node is `status: error`), proving end-to-end panic-free recovery.

### Definition of Done
- RED integration test first, then wiring; coverage ≥80% on the `run` orchestration that is unit-
  reachable.
- `just qg` green; `cargo run -- <dir>` starts and logs the listen address.
- `lib.rs`/`main.rs` docs updated; module responsibilities documented per AGENT_PROTOCOL.md §6.

## Story P0-7: Scaffold the SvelteKit frontend toolchain

Create `frontend/` as a SvelteKit + Vite + TypeScript (strict) project with TailwindCSS,
`@xyflow/svelte`, and `vitest` wired, plus shadcn-svelte initialized. Establish the inherited
commands `npm run build`, `npm run check`, `npm run lint`, `npm test`, `npm run dev`. Include one
trivial passing vitest test to prove the test runner works. No app logic beyond a placeholder route.

### Depends On: none
### Touches: frontend/**

### Acceptance Criteria
- `npm --prefix frontend run build` exits 0 and produces a build output directory.
- `npm --prefix frontend run check` (svelte-check) exits 0 with no type errors.
- `npm --prefix frontend test` runs vitest and a trivial test (e.g. `expect(1+1).toBe(2)`) passes.
- `frontend/package.json` lists `@xyflow/svelte`, `tailwindcss`, and `vitest` in its dependencies/
  devDependencies; `frontend/components.json` (shadcn-svelte) exists.
- Tailwind directives compile (a class like `text-red-500` is present in built CSS output).

### Definition of Done
- Scaffolded by `typescript-developer`; trivial test by `typescript-tester`.
- `npm run check`, `npm run lint`, `npm test`, `npm run build` all green.
- `frontend/README.md` documents the dev/build/test commands.

## Story P0-8: WS client + SvelteFlow root-level live render

Add a typed WebSocket client (`frontend/src/lib/ws.ts`) and Svelte stores that consume the CLV
envelope: ingest a `snapshot` into a `nodes` store and apply `node.upsert`/`node.remove` deltas. A
SvelteFlow canvas component (`frontend/src/lib/Graph.svelte`, used by the index route) renders one
SvelteFlow node per **root-level** Node in the store (root = node whose `parentId` is null/absent —
hierarchy expansion is Phase 1). TypeScript types for `Node`/`Edge`/`EventEnvelope` mirror
`DATA_MODEL.md` §A.2–A.4 with no `any` at the WS boundary.

### Depends On: P0-7
### Touches: frontend/src/**, frontend/tests/**

### Acceptance Criteria
- Given a fixture `snapshot` envelope with two nodes, the WS message handler populates the `nodes`
  store with length 2 and the first node's `id` matches the fixture (vitest unit test).
- Applying a `node.upsert` envelope whose node has the same `id` as an existing store node but
  `label` changed from `"foo"` to `"bar"` updates that node's `label` to `"bar"` in the store
  (vitest asserts the post-update label).
- Applying a `node.remove` envelope removes the node with that id from the store.
- A `@testing-library/svelte` component test mounting `Graph.svelte` with a store of one root node
  labeled `"alpha"` renders an element containing the text `"alpha"`; a node with a non-null
  `parentId` is **not** rendered at root.

### Definition of Done
- RED vitest/component tests first (`typescript-tester`), then implementation (`typescript-developer`);
  coverage ≥80% on `ws.ts` + store logic.
- `npm run check`, `npm run lint`, `npm test` green.
- TSDoc on exported functions and the store contract; `frontend/README.md` notes the WS event flow.
