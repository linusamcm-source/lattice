# Lattice Phase 3 — Doc-comment Extraction & Surfacing

Extracts documentation from source into each node's `docs` field (currently always `None`) and surfaces
it in the UI: hovering a node shows its description as a tooltip, and selecting a node opens a sidebar
with the full description (SPEC.md §6.5, §9.5). System-level "done" (BUILD_PLAN.md Phase 3): hovering a
documented function shows its description at every zoom level, and updating the source updates the
shown doc.

**Grounding (read this session; Phases 0–2 + run-recipe merged at `ec0d0d2`):**
- `crates/backend/src/wire.rs` — `Node.docs: Option<String>` (wire.rs:230) already exists in the model.
- `crates/backend/src/parser/mod.rs` (Rust/syn path) — `push_function(nodes, edges, file_id, path,
  ident: &syn::Ident, body: Option<&syn::Block>)` builds function nodes with `docs: None` (mod.rs:175);
  variable nodes set `docs: None` (mod.rs:208); the `file` node is built via `file_node(...)` with
  `docs: None` (mod.rs:270). `push_function` is **not** currently passed the item's `attrs`, so Rust
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
pipeline, and the existing tests are unchanged; only `docs` is now populated and rendered. Commands:
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
`string`; strip the surrounding quotes (and `"""`), returning the inner text trimmed. **TypeScript:** a
JSDoc block is a `comment` node of the form `/** ... */` immediately preceding the
`function_declaration`; strip the `/**`, `*/`, and per-line leading `*`, returning the text trimmed.
`None` when absent. Variable docs stay `None`. Panic-free; ids/structure unchanged.

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

Surface `node.docs` in the SvelteFlow UI. In `frontend/src/lib/HierarchyNode.svelte`, expose the node's
`docs` as a hover tooltip (e.g. a `title` attribute / tooltip element carrying the docs text, present
at every tier — file/function/variable). Add a `frontend/src/lib/Sidebar.svelte` that, given the
currently selected node, renders its `label` and `docs` (or an explicit "no documentation" state when
`docs` is absent); wire node selection (clicking a node selects it) through `Graph.svelte` /
`+page.svelte`. `node.docs` is already typed (`types.ts:82`); no `any` introduced.

### Depends On: P3-1, P3-2
### Touches: frontend/src/lib/Graph.svelte, frontend/src/lib/HierarchyNode.svelte, frontend/src/lib/Sidebar.svelte, frontend/src/lib/Sidebar.test.ts, frontend/src/routes/+page.svelte

### Acceptance Criteria
- A `@testing-library/svelte` test mounting a node whose `docs == "Hello docs"` renders an element
  carrying that text as a hover tooltip (assert a `title="Hello docs"` attribute or a tooltip element
  containing the text) on the node.
- Mounting `Sidebar.svelte` with a selected node `{ label: "add", docs: "Adds two numbers." }` renders
  both the label `add` and the docs text `Adds two numbers.`.
- Mounting `Sidebar.svelte` with a selected node whose `docs` is undefined renders the label and a
  "no documentation" (or equivalent empty-state) indicator, not a crash or `undefined` text.
- Selecting a node in `Graph.svelte` (simulated click) updates the sidebar to show that node's docs
  (assert the docs text appears after selection).

### Definition of Done
- RED component tests first (`typescript-tester`), then `typescript-developer`; new component logic
  ≥90% line-covered.
- `npm run check`, `npm run lint`, `npm test`, `npm run build` green (from `frontend/`).
- TSDoc on the new component contracts; `frontend/README.md` notes the doc tooltip + sidebar.
