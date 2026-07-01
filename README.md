# Lattice

> A live code & agent visualiser — watch a codebase draw itself as it is written, tested, and executed.

Lattice renders a **real-time, drill-down map of a repository** as work happens in it. Nodes represent code structure (service → module → file → function → variable) and update live as files change and tests run. Test outcomes flip nodes between states (passing / failing / running) following the TDD red→green loop, live call paths light up as code executes ("hot edges"), parameter data-flow between functions is mapped from static AST analysis, doc comments are surfaced for non-technical stakeholders, and — in multi-agent workflows — **agents are first-class nodes** with a live roster.

A single Rust binary watches a target repo, parses changed files into a structural graph, and streams diff patches over a WebSocket to a SvelteKit / SvelteFlow client that renders a lazy, zoom-gated hierarchy.

---

## Why

Existing tools split the problem and never rejoin it:

- **Dependency graph viewers** show structure, not live test state or data flow.
- **Live test runners** show pass/fail, not an architectural map.
- **Coverage dashboards** are CI-oriented and post-hoc.
- **None extract documentation** for non-technical stakeholders watching the build.

Lattice combines a real-time drill-downable architectural graph with live test status, live call-path highlighting, parameter data-flow mapping, agent attribution, *and* human-readable documentation — in one view.

---

## Architecture

The data flows **watcher → parser → graph → ws**, wired by `app::run`. A parallel path folds runtime signals (`#CLV1` stdout lines) onto the same graph.

```
                 target repo
                     │  (file changes)
              ┌──────▼───────┐
              │   watcher    │  debounced notify watcher (Rust/Python/TS)
              └──────┬───────┘
                     │ changed paths
              ┌──────▼───────┐
              │    parser    │  syn (Rust) · tree-sitter (Python/TS)
              └──────┬───────┘  → Node/Edge contribution
                     │
   #CLV1 stdout ─► collector ─► clv ─┐
   (tests, hot edges, agents)         │ correlated events
                     ┌──────▼─────────▼─┐
                     │      graph        │  in-memory source of truth
                     │  (lazy snapshot,  │  diff → node.*/edge.* patches
                     │   subtree, diff)  │
                     └──────┬───────────┘
                            │ EventEnvelope (CLV JSON)
                     ┌──────▼───────┐        ┌──────────────┐
                     │      ws       │◄──────►│   storage    │  sqlite / postgres (sqlx)
                     │ (tungstenite) │        └──────────────┘
                     └──────┬───────┘
                            │ WebSocket
                     ┌──────▼───────────────────┐
                     │  SvelteKit + SvelteFlow    │  lazy drill-down canvas
                     └───────────────────────────┘
```

### Backend (`crates/backend/src/`)

| Module | Role |
| --- | --- |
| `wire.rs` | The **CLV JSON-over-WebSocket contract**: `Node`, `Edge`, `EventEnvelope` (a discriminated union over `type`), and deterministic id helpers (`node_id` / `edge_id` / `typed_edge_id` / `agent_node_id`). Node IDs are `type:path:symbol[:child]` and stay stable across runs. Mirrors `DATA_MODEL.md` §A. |
| `parser/` | Lowers one source file to its `Node`/`Edge` contribution. `parse_source` dispatches on extension: `syn` for Rust, `tree-sitter` for Python/TypeScript; any other extension → bare `file` node. Extracts doc comments and function signatures; the Rust path also derives call and param-dataflow edges. All paths recover panic-free from syntax errors. |
| `graph.rs` | The in-memory `Graph` (source of truth). Serves a lazy root-only `snapshot`, direct children on `expand` (`subtree`), diffs a re-parsed file into patch envelopes (`apply_parsed`), and folds correlated runtime events onto the graph (`apply_clv`). |
| `clv.rs` | Read side of the `#CLV1` line protocol. `parse_clv_line` decodes one tagged stdout line into a typed `ClvEvent` (`activity` / `test` / `status` / `hotedge`), returning `None` panic-free for any malformed line. |
| `collector.rs` | Consumes a child process's stdout, feeding `#CLV1` lines to the CLV parser and correlating them onto the graph. |
| `tracing_layer.rs` | Emits hot-edge / activity signals from runtime tracing. |
| `watcher.rs` | Debounced `notify` watcher; forwards changed source paths, coalescing bursts within a debounce window. |
| `ws.rs` | `tokio-tungstenite` server. Sends a root-only snapshot on connect, then streams broadcast envelopes; answers client `snapshot` resync and `expand` requests. |
| `storage/` | `sqlx`-backed persistence with `sqlite` and `postgres` backends behind a common trait. |
| `app.rs` / `main.rs` | `app::run` joins the pipeline; `resolve_listen_addr` handles `LATTICE_ADDR`. `main.rs` is the thin binary entry. |

### Frontend (`frontend/src/lib/`)

SvelteKit + Svelte 5 + Vite + TypeScript (strict) + Tailwind v4 + `@xyflow/svelte` + shadcn-svelte, static adapter.

| File | Role |
| --- | --- |
| `types.ts` | The CLV contract mirrored `any`-free; `EventEnvelope` is a discriminated union so payloads narrow without casts. |
| `ws.ts` | The client. `parseEnvelope` validates every untrusted message (never throws, never widens to `any`); `applyEvent` is a **pure reducer** folding one envelope into an immutable `GraphState` (Maps by id) — unit-testable without a live socket. |
| `Graph.svelte` + `HierarchyNode.svelte` + `layout.ts` | The lazy SvelteFlow canvas. Expansion state is an explicit `Set<string>` acting as the render-side **zoom gate** — children are laid out only when the parent is expanded. Collapse discards transitive descendants to bound memory. |
| `RosterPanel.svelte` | Live agent roster (agent-layer view): click an agent to see nodes it touched, click a node to see which agents touched it. |
| `Sidebar.svelte` | Detail / drill-down panel. |

**CLV is the seam.** The wire protocol is versioned by the `#CLV1` sentinel. The Rust `wire.rs` types, the TS `types.ts` types, and `DATA_MODEL.md` are kept in lockstep — change all three together.

---

## Quickstart

**Prerequisites:** Rust (stable, edition 2021) + `cargo`, Node.js + `npm`, and [`just`](https://github.com/casey/just).

```bash
# Run backend (127.0.0.1:7000) and frontend dev server together.
just run            # watches the current directory
just run ../my-repo # watch a different target repo

# then open:
#   http://localhost:5173
```

Run the halves separately:

```bash
just backend [dir]   # backend only; LATTICE_ADDR overrides the listen address
just dev             # frontend dev server only (expects backend on :7000)
```

The binary is `lattice` (crate `lattice-backend`): `cargo run -p lattice-backend -- <dir>`.

---

## Install & run

Lattice ships as a **single self-contained binary**: `lattice` serves the SvelteFlow UI itself, so
you point it at a repo, open the printed address, and watch the live graph — no separate frontend
process, no Vite. The `--release` build embeds the built SvelteKit bundle into the binary.

**Install from source.** Build the frontend bundle first (the release binary embeds it), then
`cargo install` compiles and drops the `lattice` binary on your `PATH`:

```bash
npm --prefix frontend run build      # produce frontend/build/ (embedded at compile time)
cargo install --path crates/backend  # compile in release + install the `lattice` binary
```

`just release` does the same build without installing (frontend bundle → `target/release/lattice`).

**Or download a release binary.** Tagged `v*` releases publish cross-platform binaries
(Linux / macOS / Windows) to [GitHub Releases](https://github.com/linusamcm-source/lattice/releases)
via CI; download the one for your platform and put it on your `PATH`.

**Run it.** Point `lattice` at any repository and open the address it prints
(default `http://127.0.0.1:7000`):

```bash
lattice <dir>          # watch <dir> (default: the current directory)
# → prints the bound address, e.g. http://127.0.0.1:7000
# open that address in a browser to see the live graph.
```

`LATTICE_ADDR` overrides the listen address.

---

## Commands

### Backend (`just`, from repo root)

| Command | What |
| --- | --- |
| `just qg` | **Quality gate — run before every commit/merge.** = `fmt-check lint test`. |
| `just test` | `cargo test --all`. |
| `just lint` | `cargo clippy --all-targets --all-features -- -D warnings` (warnings are errors). |
| `just fmt` / `just fmt-check` | format / check formatting. |
| `just build` | `cargo build --all`. |

Tests are **inline `#[cfg(test)]` modules** inside each `src/*.rs` (no `tests/` dir). Filter with e.g. `cargo test -p lattice-backend parser::tests::` or `cargo test <substring>`.

### Frontend (from `frontend/`, or `npm --prefix frontend run <script>`)

| Command | What |
| --- | --- |
| `npm run check` | `svelte-check` typecheck (strict, must be zero errors). |
| `npm run lint` / `npm run format` | `prettier --check` / `--write`. |
| `npm test` | Vitest once. Single file: `npm test -- src/lib/ws.test.ts`; single case: `npm test -- -t "name"`. |
| `npm run coverage` | Vitest + v8 coverage. |

---

## Project layout

```
lattice/
├── crates/backend/          # single-binary Rust backend (bin: lattice)
│   └── src/
│       ├── wire.rs          # CLV wire contract + id helpers
│       ├── parser/          # syn (Rust) + tree-sitter (Python/TS)
│       ├── graph.rs         # in-memory graph, lazy snapshot/subtree/diff
│       ├── clv.rs           # #CLV1 line protocol reader
│       ├── collector.rs     # child-process stdout → CLV events
│       ├── tracing_layer.rs # hot-edge / activity signals
│       ├── watcher.rs       # debounced notify watcher
│       ├── ws.rs            # tokio-tungstenite server
│       ├── storage/         # sqlx: sqlite + postgres
│       └── app.rs / main.rs # wiring + binary entry
├── frontend/                # SvelteKit + SvelteFlow client
│   └── src/lib/             # ws client, canvas, nodes, roster, types
├── docs/
│   ├── orignal_specs/       # SPEC, BUILD_PLAN, DATA_MODEL, AGENT_PROTOCOL
│   └── sprints/             # per-phase sprint plans (phase-0 → phase-9)
└── justfile
```

---

## Build status

Built phase-by-phase per `docs/orignal_specs/BUILD_PLAN.md`:

| Phase | Feature | State |
| --- | --- | --- |
| 0 | Walking skeleton (watcher→parser→graph→ws) | ✅ |
| 1 | Hierarchy + lazy loading | ✅ |
| 2 | Multi-language tree-sitter (Python/TS) | ✅ |
| 3 | Doc-comment extraction | ✅ |
| 4 | Parameter dependency / data-flow | ✅ |
| 5 | CLV collector + live test status | ✅ |
| 6 | Hot edges (live call paths) | ✅ |
| 7 | Storage (sqlite / postgres via sqlx) | ✅ |
| 8 | Agent layer (roster + attribution) | ✅ |
| 9 | Resilience + performance | 🚧 in progress |

> Not yet built: LSP integration. Grep before assuming any subsystem exists — the spec describes more than is currently implemented.

---

## Documentation

The full design lives in `docs/orignal_specs/` — read these before non-trivial work:

- **`SPEC.md`** — product behaviour, architecture, decisions log.
- **`BUILD_PLAN.md`** — the phased build order (Phase 0 → 10). Later phases depend on earlier.
- **`DATA_MODEL.md`** — the CLV wire/DB schemas. **Contracts are fixed here.**
- **`AGENT_PROTOCOL.md`** — the `#CLV1` stdout tagging protocol and the doc-cascade rule.

---

## Contributing

- **Quality gate.** Run `just qg` (backend) and `npm run check && npm test` (frontend) before every commit or merge.
- **CLV lockstep.** Any change to the wire protocol must update `wire.rs`, `types.ts`, and `DATA_MODEL.md` together.
- **Doc-comment cascade** (`AGENT_PROTOCOL.md` §6). When you create or modify a code element, add/update its doc comment and cascade upward (function → module → crate doc). Stale higher-level docs are a defect here.
- **Validate the UI** after any frontend change — drive the running app and verify visually, don't rely on a passing unit test alone.
- Sprints run via the `team-sprint` workflow in isolated git worktrees with a **90% new-code coverage gate**, merging to `main`.
