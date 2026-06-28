# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

**Lattice** — a live code & agent visualiser. A single Rust binary watches a target repo, parses changed files into a structural graph, and streams diff patches over a WebSocket to a SvelteKit/SvelteFlow client that renders a lazy, drill-down hierarchy (service → module → file → function → variable). The full design lives in `docs/orignal_specs/` — read these before non-trivial work:

- `SPEC.md` — product behaviour, architecture, decisions log.
- `BUILD_PLAN.md` — the phased build order (Phase 0 → 10). Build in order; later phases depend on earlier.
- `DATA_MODEL.md` — the CLV wire/DB schemas. **Contracts are fixed here — don't invent schema; extend deliberately and note it.**
- `AGENT_PROTOCOL.md` — the `#CLV1` stdout tagging protocol and the doc-cascade rule (see below).

**Build status:** the skeleton is live (Phases 0–2 done, Phase 3 in progress). Most of `SPEC.md` is *not yet built* — no storage/`sqlx`, no CLV stdout collector, no agent layer, no hot edges, no LSP. Don't assume a subsystem exists because the spec describes it; grep first.

## Commands

Backend (run from repo root; uses `just`):

| Command | What |
| --- | --- |
| `just qg` | **Quality gate — run before every commit/merge.** = `fmt-check lint test`. |
| `just test` | `cargo test --all`. |
| `just lint` | `cargo clippy --all-targets --all-features -- -D warnings` (warnings are errors). |
| `just fmt` / `just fmt-check` | format / check formatting. |
| `just run [dir]` | run backend (`127.0.0.1:7000`) **and** frontend dev together; open http://localhost:5173. |
| `just backend [dir]` | backend only. `LATTICE_ADDR` overrides the listen address. |
| `just dev` | frontend dev server only (expects backend on :7000). |

A single Rust test: tests are **inline `#[cfg(test)]` modules** inside each `src/*.rs` (there is no `tests/` dir). Filter by path, e.g. `cargo test -p lattice-backend parser::tests::` or `cargo test <substring>`.

Frontend (run from `frontend/`, or `npm --prefix frontend run <script>`):

| Command | What |
| --- | --- |
| `npm run check` | `svelte-check` typecheck (strict, must be zero errors). |
| `npm run lint` / `npm run format` | `prettier --check` / `--write`. |
| `npm test` | Vitest once. Single file: `npm test -- src/lib/ws.test.ts`; single case: `npm test -- -t "name"`. |
| `npm run coverage` | Vitest + v8 coverage → `coverage/coverage-final.json`. |

Sprints run via the `team-sprint` skill (`team-sprint.config.yaml`): isolated git worktrees, **90% new-code coverage gate**, merge to `main`. Coverage command is `cargo llvm-cov`.

## Architecture

### Backend pipeline (`crates/backend/src/`)

The data flow is **watcher → parser → graph → ws**, wired by `app::run`:

- `wire.rs` — the **CLV JSON-over-WebSocket contract**: `Node`, `Edge`, `EventEnvelope` (a discriminated union over `type`), and the deterministic id helpers `node_id` / `edge_id`. Node IDs are `type:path:symbol[:child]` and must stay stable across runs so an element keeps its identity. This mirrors `DATA_MODEL.md` §A — the frontend `types.ts` mirrors it again on the TS side; **change all three together.**
- `parser/` — lowers one source file to its `Node`/`Edge` contribution. `parser::parse_source` dispatches on extension: `syn` for Rust (`parse_rust_source`), `tree-sitter` for Python/TypeScript (`treesitter.rs`); any other extension → a bare `file` node. **All paths must recover panic-free from syntax errors** (partial tree, offending node marked `error`) — this invariant is in from the moment parsing exists, per `BUILD_PLAN.md`.
- `graph.rs` — the in-memory `Graph` (source of truth). Serves a **lazy root-only `snapshot`**, direct children on `expand` (`Graph::subtree`), and diffs a re-parsed file into `node.*`/`edge.*` patch envelopes (`Graph::apply_parsed`). Lazy/zoom-gated subtree loading is the core scalability lever — don't eagerly serialise the whole tree.
- `watcher.rs` — debounced `notify` watcher; forwards changed source paths (Rust/Python/TS via `is_source_file`), coalescing bursts within `DEBOUNCE`.
- `ws.rs` — `tokio-tungstenite` server. On connect sends the root-only snapshot, then streams broadcast envelopes; answers a client `{"type":"snapshot"}` resync and `{"type":"expand","nodeId":...}` (→ `subtree` reply).
- `app.rs` — joins the above; `resolve_listen_addr` handles `LATTICE_ADDR`. `main.rs` is the thin binary entry.

### Frontend (`frontend/src/lib/`)

SvelteKit + Svelte 5 + Vite + TS (strict) + Tailwind v4 + `@xyflow/svelte` + shadcn-svelte. Static adapter. See `frontend/README.md` for the detailed event-flow walkthrough.

- `types.ts` — the CLV contract mirrored `any`-free; `EventEnvelope` is a discriminated union so payloads narrow without casts.
- `ws.ts` — the client. `parseEnvelope` validates every untrusted message (never throws, never widens to `any`, rejects unknown `type`s); `applyEvent` is a **pure reducer** folding one envelope into an immutable `GraphState` (Maps by id) — unit-testable without a live socket. `subtree` *merges* children by id; `snapshot` replaces wholesale.
- `Graph.svelte` + `HierarchyNode.svelte` + `layout.ts` — the lazy SvelteFlow canvas. Expansion state is an explicit `Set<string>` acting as the render-side **zoom gate**: children are laid out only when the parent id is in the set, even if already in the store. Collapse calls the `collapse` reducer to **discard transitive descendants** (bounds memory). Canvas mounts only in `onMount` (SvelteFlow is browser-only — never SSR it).

## Project-specific rules

- **Doc-comment cascade (required, `AGENT_PROTOCOL.md` §6).** Lattice surfaces extracted doc comments to non-technical stakeholders at every zoom level, so docs are part of the contract. When you create or modify a code element, add/update its doc comment **and cascade upward**: change a function → re-check its module doc → its crate/`lib.rs` module-level doc. Stale higher-level docs are a defect here, not a nicety.
- **Validate the UI after any frontend change.** Don't claim a UI change works from a passing unit test alone — drive the running app (`just run`) and verify visually using the **Claude-in-Chrome MCP** (`mcp__claude-in-chrome__*`) or the **Playwright CLI/MCP**: load http://localhost:5173, screenshot, confirm the change renders, and check the console for errors. Capture before/after for visual tweaks.
- **CLV is the seam.** The wire protocol is versioned by the `#CLV1` sentinel (`protocol_sentinel()`). Keep the Rust `wire.rs` types, the TS `types.ts` types, and `DATA_MODEL.md` in lockstep.
