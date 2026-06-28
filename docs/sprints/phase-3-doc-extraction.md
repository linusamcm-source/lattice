# Lattice Phase 3 — Doc-comment Extraction & Surfacing

Extracts documentation from source into each node's `docs` field (currently always `None`) and surfaces
it in the UI: hovering a node shows its description as a tooltip, and selecting a node opens a sidebar
with the full description (SPEC.md §6.5, §9.5). System-level "done" (BUILD_PLAN.md Phase 3): hovering a
documented function shows its description at every zoom level, and updating the source updates the
shown doc.

**Grounding (read this session; Phases 0–2 + run-recipe merged at `ec0d0d2`):**
- `crates/backend/src/wire.rs` — `Node.docs: Option<String>` (wire.rs:230) already exists in the model.
- `crates/backend/src/parser/mod.rs` (Rust/syn path) — `push_function(nodes, edges, file_id, path,
  ident: &syn::Ident, body: Option<&syn::Block>)` builds function nodes with `docs: None` (mod.rs:233);
  variable nodes set `docs: None` (mod.rs:295); the `file` node is built via `file_node(...)` with
  `docs: None` (mod.rs:202). `push_function` is **not** currently passed the item's `attrs`, so Rust
  doc extraction must thread `&[syn::Attribute]` (outer `///`/`#[doc]`) through it, and read the
  module-level inner doc (`//!`) from `syn::File.attrs` for the file node. (Mirrors how P1-1 threaded
  the function `Block` in.)
- `crates/backend/src/parser/treesitter.rs` (Python/TS path) — `struct LanguageConfig { function_kinds,
  binding_kinds, binding_name: fn(&TsNode,&[u8])->Option<String> }` drives the shared `extract` walk;
  function/variable nodes set `docs: None` (treesitter.rs:258). Phase 3 adds a per-language doc rule.
- `frontend/src/lib/types.ts` — `Node.docs?: string` (types.ts:82) already typed.
- `frontend/src/lib/` — `Graph.svelte` (lazy SvelteFlow render) + `HierarchyNode.svelte` (custom node:
  label + expand/collapse button, `data-testid`). No sidebar component exists yet. Tests use
  `@testing-library/svelte` (jsdom mocks in `src/test-setup.ts`).

**Scope discipline (BUILD_PLAN.md Phase 3):** doc extraction + hover/sidebar surfacing only. No
param/data-flow edges (Phase 4), no test-tracking (Phase 5). The node/edge structure, ids, lazy
pipeline, and the existing tests are unchanged; only `docs` is now populated and rendered.
**Doc-scope (deliberate subset of SPEC §6.5):** Phase 3 extracts **function** docs for all three
languages, plus the **Rust module (`//!`)** doc onto the file node. Python/TypeScript module
(file-level) docstrings and **class/interface-level** docs, and **variable-level** docs for every
language, are deferred to a later refinement — BUILD_PLAN.md's Phase-3 "Accept when" is function-level,
so the phase gate is unaffected. The "updating the source updates the shown doc" acceptance rides the
existing `apply_parsed` → `node.upsert` pipeline (a changed `docs` makes the node non-byte-equal, so
it is re-emitted — see P3-1/P3-3 ACs). Commands:
backend `just qg`/`just test`; frontend (from `frontend/`, real npm `/opt/homebrew/bin/npm`) `npm run
check`/`lint`/`test`/`build`. Coverage gate **90%** new-code.

---

## Story P3-1: Rust doc-comment extraction (syn)

Extend the Rust path in `crates/backend/src/parser/mod.rs` to populate `Node.docs`. Thread each item's
`&[syn::Attribute]` into `push_function` and extract the outer doc comments (`///` / `#[doc = "..."]`)
into the function node's `docs` (lines joined by `\n`, each line's single leading space trimmed as
rustfmt renders it; `None` when there are no doc attrs). For the `file` node, read the module-level
inner doc (`//!`, exposed as `syn::File.attrs`) into its `docs`. Variable (`let`) nodes keep `docs:
None` (Rust `let` bindings carry no doc comments). Panic-free; ids/structure unchanged.

### Depends On: none
### Touches: crates/backend/src/parser/mod.rs

### Acceptance Criteria
- Parsing `"/// Adds two numbers.\nfn add() {}"` at path `a.rs` yields a function node `fn:a.rs:add`
  whose `docs == Some("Adds two numbers.")`.
- Parsing `"/// line one\n/// line two\nfn f() {}"` yields `fn:a.rs:f` with `docs == Some("line one\nline two")`.
- Parsing `"fn bare() {}"` yields `fn:a.rs:bare` with `docs == None`.
- Parsing `"//! Module docs.\nfn f() {}"` yields the `file:a.rs` node with `docs` containing
  `"Module docs."`.
- Re-deriving docs reflects source edits: parsing `"/// v1\nfn f() {}"` then `"/// v2\nfn f() {}"` at
  the same path yields `fn:a.rs:f` with `docs == Some("v1")` then `docs == Some("v2")` (so a doc edit
  makes the node non-byte-equal and rides the existing `apply_parsed` → `node.upsert` live path).
- All existing Phase-0/1/2 parser tests still pass (structure/ids unchanged).

### Definition of Done
- `rust-tester` RED tests first, then `rust-developer`; new doc-extraction code ≥90% line-covered.
- `just qg` green (fmt + clippy -D warnings + all tests).
- `///`/`//!` docs on any new helper; cascade `parser`/`lib.rs` module docs (AGENT_PROTOCOL §6).

## Story P3-2: Python docstring + TypeScript JSDoc extraction (tree-sitter)

Extend the generic tree-sitter extractor in `crates/backend/src/parser/treesitter.rs` with a
per-language documentation rule (e.g. a `doc_for: fn(&TsNode, &[u8]) -> Option<String>` on
`LanguageConfig`, or an equivalent strategy) that populates a function node's `docs`. **Python:** the
docstring is the first statement of the function body when it is an `expression_statement` wrapping a
`string`; read that string's `string_content` child node text (this covers `"`, `'`, `"""`, `'''`,
and `r`/`b`/`f`-prefixed strings uniformly — do **not** hand-strip quotes), trimmed. **TypeScript:** a
JSDoc block is the `function_declaration`'s `prev_sibling()` of kind `comment` whose text begins with
`/**`; strip the `/**`, trailing `*/`, and per-line leading `* `, returning the text trimmed
(tree-sitter does not tokenise whitespace, so the newline between the comment and the declaration is
not an intervening node). `None` when absent. Variable + module/class docs stay `None` (deferred per
the doc-scope note). Panic-free; ids/structure unchanged.

### Depends On: none
### Touches: crates/backend/src/parser/treesitter.rs

### Acceptance Criteria
- Parsing Python `"def f():\n    \"\"\"Does a thing.\"\"\"\n    pass\n"` at `a.py` yields `fn:a.py:f`
  with `docs == Some("Does a thing.")`.
- Parsing Python `"def g():\n    pass\n"` yields `fn:a.py:g` with `docs == None`.
- Parsing TypeScript `"/** Does a thing. */\nfunction f() {}"` at `b.ts` yields `fn:b.ts:f` with
  `docs == Some("Does a thing.")`.
- Parsing TypeScript `"function g() {}"` yields `fn:b.ts:g` with `docs == None`.
- All existing Phase-2 tree-sitter tests still pass.

### Definition of Done
- RED tests first, then implementation; new doc-rule code ≥90% line-covered.
- `just qg` green.
- `///` docs on the new doc rule(s); cascade module docs (AGENT_PROTOCOL §6).

## Story P3-3: Frontend doc tooltip + selection sidebar

Surface `node.docs` in the SvelteFlow UI. **Thread docs through the layout** (`HierarchyNode` only
sees the `data` built by `buildHierarchy`): extend `HierarchyNodeData` in `frontend/src/lib/layout.ts`
with `docs?: string` and an `onSelect: (id: string) => void`, and have `buildHierarchy` copy each
node's `docs` into its data. In `HierarchyNode.svelte`, bind `data.docs` to a **`title` attribute** on
the node's content (a hover tooltip that is queryable in jsdom even under SvelteFlow's
`visibility:hidden`, present at every tier — file/function/variable), and make the label/content region
a `data-testid`-bearing clickable that calls `data.onSelect(id)`; the existing expand `<button>` must
call `event.stopPropagation()` before `data.onToggle(id)` so expanding does **not** also select. Add a
`frontend/src/lib/Sidebar.svelte` rendering the selected node's `label` and `docs` (or an explicit "no
documentation" empty state when `docs` is absent). In `Graph.svelte` hold a `selected` node id (set via
`onSelect`), pass the selected node to `Sidebar`, and render the sidebar alongside the canvas;
`+page.svelte` mounts it. `node.docs` is already typed (`types.ts:82`); no `any`. Component tests mount
`HierarchyNode` inside a `SvelteFlowProvider` (it imports `Handle`) and assert via `title` /
`data-testid` + text — not SvelteFlow node-selection internals — matching the existing harness
(`Graph.test.ts` already documents the `visibility:hidden` / `data-testid` approach).

### Depends On: P3-1, P3-2
### Touches: frontend/src/lib/Graph.svelte, frontend/src/lib/HierarchyNode.svelte, frontend/src/lib/layout.ts, frontend/src/lib/Sidebar.svelte, frontend/src/lib/Sidebar.test.ts, frontend/src/lib/Graph.test.ts, frontend/src/routes/+page.svelte

### Acceptance Criteria
- A `@testing-library/svelte` test mounting `HierarchyNode` (inside a `SvelteFlowProvider`) with
  `data.docs == "Hello docs"` renders an element carrying `title="Hello docs"`.
- Mounting `Sidebar.svelte` with a selected node `{ label: "add", docs: "Adds two numbers." }` renders
  both the label `add` and the docs text `Adds two numbers.`.
- Mounting `Sidebar.svelte` with a selected node whose `docs` is undefined renders the label and a
  "no documentation" (or equivalent empty-state) indicator — not a crash or the literal `undefined`.
- In `Graph.svelte`, clicking a node's `data-testid` content region invokes `onSelect` with that node's
  id and the sidebar shows that node's docs; clicking the expand **button** does NOT change the
  selection (the button `stopPropagation`s).
- Applying a `node.upsert` whose node carries updated `docs` for the currently-selected node updates
  the rendered sidebar text (proves the live "updating the source updates the shown doc" path through
  the existing store → render pipeline).

### Definition of Done
- RED component tests first (`typescript-tester`), then `typescript-developer`; new component logic
  ≥90% line-covered.
- `npm run check`, `npm run lint`, `npm test`, `npm run build` green (from `frontend/`).
- TSDoc on the new component contracts; `frontend/README.md` notes the doc tooltip + sidebar.
