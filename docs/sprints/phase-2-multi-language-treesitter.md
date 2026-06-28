# Lattice Phase 2 — Multi-language (tree-sitter)

Adds a `tree-sitter` parsing path so Python and TypeScript files produce the **same** structural
node/edge model the Rust `syn` path already produces (file → function → variable, `contains` edges),
flowing through the existing lazy pipeline unchanged. System-level "done" (BUILD_PLAN.md Phase 2):
the same hierarchy + lazy-load (snapshot = files, expand → functions, expand → variables) works on
`.py` and `.ts` files, not just `.rs`.

**Grounding (read this session; Phases 0+1 merged at `eaa9580`):**
- `crates/backend/src/parser/mod.rs` — `pub fn parse_rust_source(path: &str, source: &str) ->
  ParsedFile { nodes, edges }` (syn). Builds a `file` node via `file_node(id,label,status)` (which
  sets `meta: None`), one `function` node per fn/method via `push_function` (sets
  `meta.language = Some("rust")`, `meta.file_path`, `meta.range`, and a `contains` edge from the
  file), and `variable` nodes per `let` binding via `push_variables`/`collect_pattern_idents` (also
  `meta.language = "rust"`). Ids: `node_id(NodeType::{File,Function,Variable}, path, symbol)` from
  `wire.rs` (`file:<path>`, `fn:<path>:<name>`, `var:<path>:<fn>:<name>`). Panic-free on bad syntax:
  returns a single `file` node with `status == error`.
- `crates/backend/src/watcher.rs` — `pub fn is_rust_file(path: &Path) -> bool` matches only the `rs`
  extension; the `notify` callback filters on it (watcher.rs:84). Tests in `watcher::tests`.
- `crates/backend/src/app.rs` — `ingest_file` calls `parse_rust_source(&rel, &source)` (app.rs:80);
  `run()`'s initial-parse loop filters `path.extension() == Some("rs")`. Three e2e tests use `.rs`.
- `crates/backend/src/wire.rs` — `NodeType::{File,Function,Variable}`, `node_id`, `Node.meta.language`.

**Design (refined):** add a config-driven generic tree-sitter extractor + a `parse_source(path,
source) -> ParsedFile` dispatcher (by extension: `rs` → `parse_rust_source`; `py`/`ts` → the
tree-sitter path; other → a bare `file` node). The extractor walks the parse tree and, per a small
per-language `LanguageConfig`, emits `function` nodes for the language's function node-kinds as
children of the file, and `variable` nodes for the language's local bindings as children of their
enclosing function. **Name extraction is per-language** (the languages don't share one binding
node-kind): function nodes use the grammar's `name` field (Python `function_definition` and TS
`function_declaration` both have one); variable nodes use a per-language rule (Python: an `assignment`
whose `left` is a single `identifier`; TS: a `variable_declarator`'s `name` field). Ids, `contains`
edges, and `meta` mirror the Rust path (function/variable nodes carry `meta.language`; the file node's
`meta` stays `None`).

**Range conversion:** tree-sitter positions use **0-based rows**; the CLV `Range` uses **1-based
lines** (wire.rs `startLine` doc: "1-based"). The extractor converts `row + 1` for start/end lines
(columns stay 0-based, as in the syn path) so ranges match the existing contract.

**Error model (partial recovery, per SPEC.md §11.1 / BUILD_PLAN.md Phase 0 "the rest stays live"):**
the tree-sitter path is panic-free and **still emits every function/variable it can recover** from the
(partial) tree; additionally, when the parse tree's root reports an error
(`root_node().has_error()`), the `file` node's `status` is set to `error` as a coarse "this file has a
syntax problem" flag. This deliberately differs from the syn path's all-or-nothing (which returns
only the file node) — recovered siblings stay live. Precise per-offending-node error marking is
refined in Phase 9 (resilience).

**⚠️ Integration risk (verify FIRST in P2-1):** the `tree-sitter` core crate and the
`tree-sitter-python` / `tree-sitter-typescript` grammar crates must be ABI-compatible (a grammar
built against a different `tree-sitter` major/minor will fail to link). P2-1 must pin a mutually
compatible set and confirm `cargo build` links before writing extraction logic.

**Scope discipline:** Python + TypeScript only (Phase 2). No doc extraction (Phase 3), no
param/data-flow edges (Phase 4). The Rust `syn` path, all existing node ids, and the frontend are
unchanged (the lazy render already renders any file/function/variable nodes). Commands: backend
`just qg` / `just test`; coverage gate **90%** new-code (cargo-llvm-cov).

---

## Story P2-1: tree-sitter generic extractor + Python support

Add `tree-sitter` + `tree-sitter-python` to `crates/backend/Cargo.toml` (pinned to a mutually
compatible, linking set — **verify `cargo build` links before any extraction code**). Add a generic
extractor (e.g. `crates/backend/src/parser/treesitter.rs`, module-private to `parser`) driven by a
`LanguageConfig` describing function node-kinds, local-binding node-kinds, and name extraction, and a
`parse_python(path, source) -> ParsedFile` entry that uses it. The extractor emits the same model as
the Rust path: a `file` node, `function` nodes (`fn:<path>:<name>`, name from the
`function_definition` `name` field, parent the file, `meta.language = "python"`, `meta.range` from the
node's start/end with **`row + 1`** for 1-based lines, plus a `contains` edge), and `variable` nodes
(`var:<path>:<fn>:<name>`, parent the function, `contains` edge). **Python locals** are each
`assignment` statement inside a function body whose `left` is a single `identifier` — the variable
name is that identifier's text; tuple/multiple-target assignments are out of Phase-2 scope. Per the
design's **error model**, the extractor still emits every recovered function/variable; if the parsed
tree's root node reports an error (`root_node().has_error()`), the `file` node's `status` is `error`.
Panic-free: never `.unwrap()`/`panic!` outside `#[cfg(test)]`.

### Depends On: none
### Touches: crates/backend/src/parser/mod.rs, crates/backend/src/parser/treesitter.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- Parsing Python `"def foo():\n    pass\n\ndef bar():\n    pass\n"` at path `a.py` yields a `file:a.py`
  node plus exactly two `function` nodes `fn:a.py:foo` and `fn:a.py:bar`, each with
  `parentId == "file:a.py"` and a `contains` edge from `file:a.py`.
- Parsing Python `"def f():\n    x = 1\n    y = 2\n"` yields `variable` nodes `var:a.py:f:x` and
  `var:a.py:f:y`, each with `parentId == "fn:a.py:f"` and a `contains` edge from `fn:a.py:f`.
- Every `function`/`variable` node from the Python path has `meta.language == "python"` and
  `meta.range.startLine > 0`.
- Parsing malformed Python `"def (:\n"` does **not** panic and returns a `ParsedFile` whose `file:`
  node has `status == error` (assert via `std::panic::catch_unwind` returning `Ok`).

### Definition of Done
- `rust-tester` RED tests first, then `rust-developer`; the new tree-sitter extractor code is ≥90%
  line-covered.
- `just qg` green (fmt + clippy -D warnings + all tests, including unchanged Phase-0/1 tests).
- `cargo build` links with the pinned tree-sitter crate set (no ABI/link error).
- `//!`/`///` docs on the new module + entry; cascade `parser` `//!`/`lib.rs` docs (AGENT_PROTOCOL §6).

## Story P2-2: TypeScript support via the generic extractor

Add `tree-sitter-typescript` to `crates/backend/Cargo.toml` (compatible with the P2-1 tree-sitter
core) and a `parse_typescript(path, source) -> ParsedFile` entry reusing the P2-1 generic extractor
with a TypeScript `LanguageConfig`. **Phase-2 scope: top-level `function_declaration` only** (name via
its `name` field); class methods and arrow functions bound in a `variable_declarator` are deferred to
a later phase (they need a different name-extraction path and are not exercised here). **Locals:**
each `variable_declarator` inside a function body, name from its `name` field (covers `const`/`let`
via `lexical_declaration`). `meta.language == "typescript"`, `row + 1` line conversion, same ids,
`contains` edges, and the same partial-recovery error model as P2-1.

### Depends On: P2-1
### Touches: crates/backend/src/parser/treesitter.rs, crates/backend/src/parser/mod.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- Parsing TypeScript `"function foo() {}\nfunction bar() {}\n"` at path `b.ts` yields `file:b.ts`
  plus exactly two `function` nodes `fn:b.ts:foo` and `fn:b.ts:bar`, each with `parentId ==
  "file:b.ts"` and a `contains` edge.
- Parsing TypeScript `"function f() { const x = 1; let y = 2; }"` yields `variable` nodes
  `var:b.ts:f:x` and `var:b.ts:f:y`, each with `parentId == "fn:b.ts:f"` and a `contains` edge.
- Every `function`/`variable` node from the TS path has `meta.language == "typescript"` and
  `meta.range.startLine > 0`.
- Parsing malformed TypeScript `"function ("` does **not** panic and returns a `ParsedFile` whose
  `file:` node has `status == error`.

### Definition of Done
- RED tests first, then implementation; new TS-config/extractor code ≥90% line-covered.
- `just qg` green; `cargo build` links with the added grammar.
- `///` docs on the TS entry/config; cascade module docs (AGENT_PROTOCOL §6).

## Story P2-3: Language dispatch + watcher/app wiring + multi-language e2e

Add `pub fn parse_source(path: &str, source: &str) -> ParsedFile` in `parser/mod.rs` that routes by
extension: `rs` → `parse_rust_source`, `py` → `parse_python`, `ts` → `parse_typescript`, any other →
a bare `file` node (`status: unknown`, no children). **Rename** `is_rust_file` →
`pub fn is_source_file(path: &Path) -> bool` accepting `rs`/`py`/`ts`; update the `notify` callback to
call it and rename the existing unit test `is_rust_file_accepts_rs_and_rejects_others` →
`is_source_file_accepts_source_exts_and_rejects_others` (now asserting `.py`/`.ts`/`.rs` accepted and
`.txt`/`.rs.bak` rejected). Route `app.rs` through `parse_source` (in `ingest_file`) and replace the
`run()` initial-parse `rs`-only filter with `is_source_file`. The Rust path, its ids, and all
Phase-0/1 tests remain green.

### Depends On: P2-2
### Touches: crates/backend/src/parser/mod.rs, crates/backend/src/watcher.rs, crates/backend/src/app.rs, crates/backend/Cargo.toml

### Acceptance Criteria
- `parse_source("x.rs", "fn f() {}")` contains `fn:x.rs:f`; `parse_source("x.py", "def f():\n    pass\n")`
  contains `fn:x.py:f`; `parse_source("x.ts", "function f() {}")` contains `fn:x.ts:f`;
  `parse_source("x.md", "# hi")` returns only a `file:x.md` node (no function nodes).
- The watcher filter accepts paths ending `a.py`, `a.ts`, and `a.rs`, and rejects `notes.txt`.
- A `run()` integration test pointed at a tempdir containing `m.py` with `def alpha():\n    pass\n`:
  a connected WS client's initial `snapshot` contains `file:m.py` but not `fn:m.py:alpha`; sending
  `{"type":"expand","nodeId":"file:m.py"}` yields a `subtree` whose `payload.nodes` includes
  `fn:m.py:alpha`.
- The same `run()` flow for a `t.ts` file with `function alpha() {}` yields `file:t.ts` in the
  snapshot and `fn:t.ts:alpha` in the `expand` subtree.

### Definition of Done
- RED tests first (incl. the two multi-language e2e tests), then implementation; new dispatch/wiring
  code ≥90% line-covered.
- `just qg` green; all Phase-0/1 e2e (the `.rs` flow) still pass.
- `///`/`//!` docs for `parse_source`, `is_source_file`, and the routed `app` paths; cascade module
  docs (AGENT_PROTOCOL §6).
