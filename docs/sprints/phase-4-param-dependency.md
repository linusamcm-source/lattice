# Lattice Phase 4 — Parameter Dependency Mapping (static, AST-derived)

Populates each `function` node's `signature` (params + return), derives **control-flow** `calls`
edges and **data-flow** `param_source` / `data_flows_from` edges (currently no `calls` or data-flow
edges are produced — only `contains`), and renders + filters those edges on the SvelteFlow canvas
(which currently draws **no** edges at all). System-level "done" (BUILD_PLAN.md Phase 4): a function
consuming another's return value shows a data-flow edge; editing the dependency re-derives the edge on
save; control-flow and data-flow can be toggled independently (SPEC.md §6.4, §9.3).

**Grounding (read this session; Phases 0–3 merged at `39f9ffd`):**
- `crates/backend/src/wire.rs` — the wire contract is **already complete**, Phase 4 only *populates* it:
  `EdgeKind` has `Calls`, `ParamSource`, `DataFlowsFrom` (wire.rs:94–108, serialised `calls` /
  `param_source` / `data_flows_from`); `Signature { params: Vec<Param>, returns: String }` and
  `Param { name, param_type (serde "type") }` exist (wire.rs:157–174); `Node.signature: Option<Signature>`
  exists (wire.rs:233) and is **always `None`** today. No new wire types, no `DATA_MODEL.md` change.
- `crates/backend/src/parser/mod.rs` (Rust/syn) — the item walk at mod.rs:95–131 visits
  `syn::Item::Fn` / `Item::Impl` methods / `Item::Trait` methods and calls `push_function(nodes, edges,
  file_id, path, ident, attrs, body)` (mod.rs:230). The `syn::Signature` (`item_fn.sig`,
  `method.sig`) holds `inputs: Punctuated<FnArg>` and `output: ReturnType` but is **not** currently
  threaded in — only `&sig.ident`, `&...attrs`, and the body are. `push_function` builds the node with
  `signature: None` (mod.rs:249) and pushes only a `Contains` edge (mod.rs:256–262). The body
  (`Option<&syn::Block>`) is walked for `let` bindings by `push_variables` (mod.rs:279) — the same
  block holds the call expressions Phase 4 needs.
- `crates/backend/src/parser/treesitter.rs` (Python/TS) — `struct LanguageConfig { function_kinds,
  binding_kinds, binding_name, doc_for }` (treesitter.rs:36–53) drives the shared `extract` walk
  (treesitter.rs:206); Phase 3 added `doc_for` as a per-language `fn(&TsNode,&[u8])->Option<String>`.
  Phase 4 adds a per-language **signature** reader the same way. Python fns are `function_definition`,
  TS fns are `function_declaration` (treesitter.rs:78,148).
- `crates/backend/src/graph.rs` — `Graph::apply_parsed` diffs a re-parsed file's nodes/edges into
  `node.*` / `edge.*` envelopes and tracks `file_edges` per file (graph.rs:189–221), so **any** edge a
  parser emits for a file already rides the existing upsert/remove live path. No graph change needed
  beyond what the parser produces.
- `frontend/src/lib/ws.ts` — the `edges` derived store already exists (ws.ts:137) and `subtree` /
  `edge.upsert` already merge edges into `GraphState.edges` (ws.ts:66–83); the data reaches the client.
- `frontend/src/lib/layout.ts` — `buildHierarchy` emits **only positioned nodes**; it ignores edges
  entirely. `frontend/src/lib/Graph.svelte` keeps `flowEdges` as a permanently-empty
  `$state.raw<FlowEdge[]>([])` (Graph.svelte:60,119). **The canvas renders no edges today.**
- `frontend/src/lib/types.ts` mirrors the contract already: `EdgeKind` union with `param_source` /
  `data_flows_from` (types.ts:20–27), `NodeSignature` / `SignatureParam`, `Node.signature?`
  (types.ts:83). No TS schema change.

**Scope discipline (BUILD_PLAN.md Phase 4):** signature extraction + `calls`/data-flow edge derivation
+ frontend edge render/colour/filter only. **Static AST only — no runtime value inspection** (that is
the whole point of §6.4). No Phase 5 CLV, no Phase 6 hot edges. Node ids/structure, the lazy pipeline,
and every existing Phase 0–3 test stay unchanged; only `signature` is now populated and new
non-`contains` edges are added. Parser stays **panic-free** on malformed source (partial tree).

**Language-scope (deliberate subset, mirrors Phase 3's documented subset):** function **signatures**
are extracted for all three languages (Rust, Python, TypeScript). **Edge derivation** (`calls` +
`param_source` / `data_flows_from`) is scoped to **Rust** this phase — it is the syn path with the
richest static information and is where the BUILD_PLAN "a function consuming another's return value
shows a data-flow edge" acceptance is demonstrated. Python/TypeScript call + data-flow edge derivation
is deferred to a later refinement; BUILD_PLAN's Phase-4 gate is language-agnostic in wording and met on
Rust. The frontend edge render/filter (P4-4) is language-agnostic — it draws whatever edges the store
holds — so it works automatically once Python/TS edges land later.

**Data-flow derivation — explicit simplifying assumptions (P4-3):** resolution is **intra-file,
same-language, by bare callee name** to a sibling `function` node in the same file (no cross-file, no
type-based or trait resolution, no imports). Two static patterns are derived inside one function body:
(a) **nested call** `outer(inner())` — `inner`'s return flows into `outer`'s parameter; (b)
**single let-binding indirection** `let v = inner(); outer(v)` — `v` bound to `inner()`'s return, then
passed to `outer`. Method-call chains, aliasing/reassignment, multi-hop binding, and tuple
destructuring are out of scope (noted, not derived). Each derived dependency emits the **dual** edges:
`inner --data_flows_from--> outer` and `outer --param_source--> inner` (§6.4 / DATA_MODEL §A.3).

**Commands:** backend `just qg` / `just test`; frontend (from `frontend/`, real npm
`/opt/homebrew/bin/npm`) `npm run check` / `lint` / `test` / `build`. Coverage gate **90%** new-code.
Target branch `main`.

---

## Story P4-1: Rust function signature extraction (syn)

Thread the `syn::Signature` into the Rust function lowering and populate `Node.signature`. Pass the
fn's `&syn::Signature` (from `item_fn.sig` / `method.sig` at mod.rs:97–131) into `push_function`
(mod.rs:230) and build a `wire::Signature`: for each `syn::FnArg::Typed`, a `Param { name, param_type }`
where `name` is the binding pattern rendered as source text and `param_type` is the type rendered from
tokens (whitespace-collapsed so `i32` / `Credentials` round-trip cleanly); `returns` is the
`ReturnType` rendered (`""` for the default/unit return). `FnArg::Receiver` (`self`) is skipped (not a
data parameter). A fn with no typed params and unit return yields `Signature { params: [], returns: "" }`
(still `Some`, so the node carries a signature). Variable nodes keep `signature: None`. Panic-free;
ids/structure unchanged. **Type rendering:** render `syn::Type` / pattern via
`quote::ToTokens::to_token_stream().to_string()` then collapse the token whitespace (token printing
inserts spaces, e.g. `Vec < T >`) to a clean form (`Vec<T>`); this needs `quote` as a direct dep —
add `quote = "1"` to `crates/backend/Cargo.toml` (it is already a transitive dep via syn's default
`printing` feature, so it is in the lock; `proc-macro2` with `span-locations` is already direct).

### Depends On: none
### Touches: crates/backend/src/parser/mod.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- Parsing `"fn add(a: i32, b: i32) -> i32 { a + b }"` at `a.rs` yields `fn:a.rs:add` whose `signature`
  is `Some` with `params == [Param{name:"a",param_type:"i32"}, Param{name:"b",param_type:"i32"}]` and
  `returns == "i32"`.
- Parsing `"fn noop() {}"` yields `fn:a.rs:noop` with `signature == Some(Signature{ params: [], returns: "" })`.
- Parsing `"struct S; impl S { fn m(&self, x: u8) -> bool { true } }"` yields `fn:a.rs:m` with
  `params == [Param{name:"x",param_type:"u8"}]` (the `&self` receiver is excluded) and `returns == "bool"`.
- A signature edit re-derives: parsing `"fn f(a: i32) {}"` then `"fn f(a: i64) {}"` at the same path
  yields `fn:a.rs:f` with `param_type "i32"` then `"i64"` (the changed signature makes the node
  non-byte-equal so it rides `apply_parsed` → `node.upsert`).
- All existing Phase 0–3 parser tests still pass (ids/structure/docs unchanged; only `signature` is now populated).

### Definition of Done
- `rust-tester` RED tests first, then `rust-developer`; new signature-extraction code ≥90% line-covered.
- `just qg` green (fmt + clippy -D warnings + all tests).
- `///` docs on any new helper (e.g. the type-rendering fn); cascade `parser`/`lib.rs` module docs (AGENT_PROTOCOL §6).

## Story P4-2: Python + TypeScript function signature extraction (tree-sitter)

Add a per-language signature reader to `LanguageConfig` (treesitter.rs:36), mirroring how Phase 3 added
`doc_for`: a field `signature_of: fn(&TsNode, &[u8]) -> Option<Signature>` populated for the Python and
TypeScript configs and read in `extract` (treesitter.rs:206) when a `function_kinds` node is emitted.
**Python** (`function_definition`): read the `parameters` child for each `identifier` /
`typed_parameter` (name, plus annotation text after `:` when present, else `""`); `returns` is the
`return_type` annotation text after `->` when present, else `""`; `self` / `cls` first params are
excluded. **TypeScript** (`function_declaration`): read `formal_parameters` for each
`required_parameter` / `optional_parameter` (name = the `identifier` pattern, type = the
`type_annotation` text after `:` when present, else `""`); `returns` is the function's `type_annotation`
return text when present, else `""`. Absent annotations yield empty-string types (Python/TS are
optionally typed) — never a panic. Variable nodes keep `signature: None`. Panic-free; ids/structure unchanged.

### Depends On: none
### Touches: crates/backend/src/parser/treesitter.rs

### Acceptance Criteria
- Parsing Python `"def add(a: int, b: int) -> int:\n    return a + b\n"` at `a.py` yields `fn:a.py:add`
  with `params == [Param{name:"a",param_type:"int"}, Param{name:"b",param_type:"int"}]` and `returns == "int"`.
- Parsing Python `"def f(x):\n    pass\n"` yields `fn:a.py:f` with `params == [Param{name:"x",param_type:""}]`
  and `returns == ""` (untyped param/return → empty type strings, `signature` still `Some`).
- Parsing TypeScript `"function add(a: number, b: number): number { return a + b; }"` at `b.ts` yields
  `fn:b.ts:add` with `params == [Param{name:"a",param_type:"number"}, Param{name:"b",param_type:"number"}]`
  and `returns == "number"`.
- Parsing TypeScript `"function g(x) {}"` yields `fn:b.ts:g` with `params == [Param{name:"x",param_type:""}]`
  and `returns == ""`.
- All existing Phase-2/3 tree-sitter tests still pass.

### Definition of Done
- RED tests first, then implementation; new signature-reader code ≥90% line-covered.
- `just qg` green.
- `///` docs on the new `signature_of` rules; cascade module docs (AGENT_PROTOCOL §6).

## Story P4-3: Rust call edges + data-flow edge derivation (syn)

Derive non-`contains` edges from Rust function bodies. Walk each function body (the `syn::Block` already
passed to `push_function`, mod.rs:263) for call expressions and emit, for callees that resolve **by bare
name to a same-file `function` node**: (1) a `calls` edge `caller --calls--> callee` (control flow,
deduplicated per caller/callee pair). Then derive **data-flow** edges from the two static patterns in
the Language-scope note — nested call `outer(inner())` and single let-binding indirection
`let v = inner(); outer(v)` — emitting the dual pair `inner --data_flows_from--> outer` and
`outer --param_source--> inner` (deduplicated). **Edge-id uniqueness (load-bearing — verified
against the contract):** the existing `edge_id(src, dst)` helper is `format!("e:{src}->{dst}")`
(wire.rs:368) — it keys on the **endpoint pair only, not the kind** — and `apply_parsed` stores edges
in an id-keyed map (`self.edges.insert(edge.id, …)`, graph.rs:200). So a `calls` edge and a
`data_flows_from` edge on the **same ordered pair** `X→Y` (reachable: `fn x() -> i32 { y(3) }` gives
`x --calls--> y`, while `y(x())` elsewhere gives `x --data_flows_from--> y`) would produce the **same
id** and silently overwrite each other — the "distinct ordered pairs" assumption does **not** hold in
general. Therefore the **new** edge kinds (`calls`, `param_source`, `data_flows_from`) MUST use a
**kind-qualified id** — add a `typed_edge_id(src, dst, kind)` helper to `wire.rs` returning
`e:<src>-><dst>:<kind>` (kind = the serde string `calls` / `param_source` / `data_flows_from`) so every
edge id is unique per `(source, target, kind)`. This is a **deliberate extension of DATA_MODEL §A.1**
(whose `e:<src>-><dst>` form contemplates only one edge per ordered pair — note it in the wire doc/Rust
doc-comment). `contains` edges keep the unqualified `edge_id(src, dst)` form **unchanged** so every
existing Phase 0–3 edge-id assertion (e.g. graph.rs:358,379,396) still passes. Unresolved callees (names with no
matching same-file function node — external/std/imported) are **skipped silently**, not errored.
Panic-free on malformed bodies (partial/`error` recovery unchanged). Existing `contains` edges and all
node structure stay byte-identical.

### Depends On: P4-1
### Touches: crates/backend/src/parser/mod.rs, crates/backend/src/wire.rs

### Acceptance Criteria
- Parsing `"fn a() {}\nfn b() { a(); }"` at `x.rs` yields a `calls` edge with `source == fn:x.rs:b`,
  `target == fn:x.rs:a`, `kind == Calls` (b calls a). No data-flow edge (no value consumed).
- **Edge-id collision is avoided:** parsing `"fn y(v: i32) -> i32 { v }\nfn x() -> i32 { y(3) }\nfn m() { y(x()); }"`
  at `x.rs` yields **both** a `calls` edge `x --calls--> y` and a `data_flows_from` edge
  `x --data_flows_from--> y` as **two distinct edges with distinct ids** (the `calls` id ends `:calls`,
  the data-flow id ends `:data_flows_from`); neither overwrites the other in the graph.
- `typed_edge_id("fn:x.rs:a", "fn:x.rs:b", EdgeKind::Calls) == "e:fn:x.rs:a->fn:x.rs:b:calls"`, and the
  three new edge kinds produce distinct ids for the same endpoint pair.
- Parsing nested-call `"fn inner() -> i32 { 1 }\nfn outer(v: i32) {}\nfn f() { outer(inner()); }"` at
  `x.rs` yields both `fn:x.rs:inner --data_flows_from--> fn:x.rs:outer` and
  `fn:x.rs:outer --param_source--> fn:x.rs:inner` (inner's return flows into outer's param), plus the
  `calls` edges for `f→outer` and `f→inner`.
- Parsing let-binding indirection `"fn inner() -> i32 { 1 }\nfn outer(v: i32) {}\nfn f() { let t = inner(); outer(t); }"`
  yields the same `data_flows_from` / `param_source` dual between `inner` and `outer`.
- Parsing `"fn b() { external_lib(); }"` (no same-file `external_lib` function node) yields **no**
  `calls` or data-flow edge for it (unresolved callee skipped) and does not panic.
- Parsing malformed Rust (e.g. `"fn b( { a("`) recovers without panic and still emits the file node;
  no spurious data-flow edges.
- All existing Phase 0–3 parser tests still pass; `contains` edges for the same input are unchanged.

### Definition of Done
- `rust-tester` RED tests first, then `rust-developer`; new edge-derivation code ≥90% line-covered, including the unresolved-callee and malformed-body paths.
- `just qg` green.
- `///` docs on the call-walk / data-flow helpers stating the intra-file/by-name assumptions; cascade `parser`/`lib.rs` module docs (AGENT_PROTOCOL §6).

## Story P4-4: Frontend edge rendering + kind colour + control/data-flow filter

Render graph edges on the SvelteFlow canvas (none are drawn today) and let the user toggle control-flow
vs data-flow independently. In `frontend/src/lib/layout.ts` add a `buildEdges(graphEdges: Edge[],
visibleNodeIds: ReadonlySet<string>, filter): FlowEdge[]` that returns a SvelteFlow edge **only when
both `source` and `target` are visible** (ids present in the set of nodes `buildHierarchy` emitted),
colour-/class-keyed by `kind`: `calls` (control flow) one colour, `param_source` + `data_flows_from`
(data flow) another; `contains` edges are **not** drawn (containment is already shown by the column
layout — drawing it would clutter). In `Graph.svelte` derive the visible-node-id set from the
`buildHierarchy` result, subscribe to the existing `edges` store (ws.ts:137), compute `flowEdges` via
`buildEdges`, and add a control (shadcn-svelte) with two independent toggles — **Control flow** (calls)
and **Data flow** (param_source/data_flows_from) — that include/exclude each edge class; both default
on. No `any`; lazy discipline unchanged (collapsing a parent removes its nodes from the visible set, so
its edges drop out automatically).

### Depends On: P4-3
### Touches: frontend/src/lib/layout.ts, frontend/src/lib/Graph.svelte, frontend/src/lib/layout.test.ts, frontend/src/lib/Graph.test.ts

### Acceptance Criteria
- A `buildEdges` unit test: given a `calls` edge whose `source` and `target` are both in the visible set
  returns one FlowEdge carrying a class/style identifying it as control-flow; given a `param_source`
  edge returns one identifying it as data-flow.
- `buildEdges` returns **no** FlowEdge for an edge whose `source` or `target` is absent from the visible
  set (endpoint not on canvas), and **no** FlowEdge for a `contains` edge.
- With the **Control flow** toggle off, `buildEdges` (or the Graph wiring) excludes `calls` edges while
  still returning `param_source`/`data_flows_from`; with **Data flow** off it excludes the data-flow
  edges while still returning `calls` — the two toggle independently.
- A `@testing-library/svelte` test on `Graph.svelte`: after the store holds two visible function nodes
  and a `calls` edge between them, the rendered SvelteFlow edge count reflects the edge; toggling
  Control flow off removes it.
- `npm run check`, `npm run lint`, `npm test`, `npm run build` green (from `frontend/`).

### Definition of Done
- RED component/unit tests first (`typescript-tester`), then `typescript-developer`; new edge-build + toggle logic ≥90% line-covered.
- `npm run check` / `lint` / `test` / `build` green.
- TSDoc on `buildEdges` and the toggle contract; `frontend/README.md` notes edge rendering + the control/data-flow filter.
