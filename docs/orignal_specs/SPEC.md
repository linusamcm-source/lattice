# Lattice — Technical Specification

> **Product:** *Lattice* — a live code & agent visualiser.
> **Status:** Design complete, ready for build handoff.
> **Audience:** Implementation engineer or autonomous coding agent.
> **Wire protocol:** **CLV** (*Code-Live-View*), v1 — `#CLV1` sentinel. Stable independent of the product name.
> **Contracts (schemas):** see `DATA_MODEL.md`. **Build order:** see `BUILD_PLAN.md`. **Agent rules:** see `AGENT_PROTOCOL.md`.

---

## 1. Summary

Lattice renders a **live, interactive map of a codebase as it is written and tested**. Nodes represent code structure (service → module → file → function → variable) and update in real time as files change and tests run. Test outcomes flip nodes between states (passing / failing / running) following a TDD red→green loop, and **live call paths light up as code executes**.

Beyond the call graph, Lattice maps **parameter data-flow** between functions (what feeds an input, what consumes an output) using static AST analysis. **Doc comments are extracted and surfaced** so non-technical stakeholders can read what each part does. In multi-agent workflows, **agents are first-class nodes** with a live roster: click an agent to see every node it touched, or click a node to see which agents touched it.

The tool watches the repo continuously and automatically — developers and agents work and run tests as normal; Lattice reflects what's happening.

---

## 2. Motivation

Existing tools split the problem and never rejoin it:
- **Dependency graph viewers** show structure, not live test state or data flow.
- **Live test runners** show pass/fail, not an architectural map.
- **Coverage dashboards** are CI-oriented and post-hoc.
- **None extract documentation** for non-technical stakeholders watching the build.

None combine a real-time, drill-downable architectural graph with live test status, live call-path highlighting, parameter data-flow mapping, agent attribution, *and* human-readable documentation. That's the product.

---

## 3. Goals / Non-goals

**Goals**
- Real-time feedback on what code is being touched, at any zoom level from service to variable.
- Live TDD state per node (red / green / running) from test output.
- Live call-path highlighting (hot edges) from runtime tracing.
- **Parameter dependency mapping** — static AST-derived data flow between functions.
- **Doc-comment extraction and surfacing** — legible to non-technical stakeholders.
- Language-agnostic via a generic parser.
- Scales from a single script to a large monorepo.
- Track *which agent* (or human) authored/modified each node, with a live roster.
- Zero manual setup per run once installed.
- Cross-platform single-binary install.

**Non-goals (v1)**
- Not an editor/IDE replacement — it observes, it doesn't edit.
- Not a CI dashboard — local-first; CI is later.
- Not a profiler of the user's code (Lattice's *own* performance is an NFR, §11).
- **No runtime value tracking** — data-flow edges are static/structural, not value traces (§6).
- No semantic/vector search — relational only (revisit later).

---

## 4. Architecture overview

```mermaid
flowchart LR
    subgraph Repo["Target repository (any language)"]
        FS[(Source files)]
        TR[Test runners / agents\nemit CLV-tagged stdout]
        RT[Runtime tracer\ncall-stack -> hot edges]
    end

    subgraph Backend["Rust backend (single binary)"]
        W[File watcher\nnotify]
        P[Parser layer\nsyn + tree-sitter\nAST + docs + params]
        C[CLV stdout collector\nparallel per-process readers]
        G[In-memory graph model]
        DB[(Storage\nSQLite | Postgres)]
        WS[WebSocket server\ntokio-tungstenite]
    end

    subgraph Frontend["SvelteFlow web client"]
        UI[Hierarchical canvas\nlazy + zoom-gated\ndocs on hover]
        AG[Agent layer + live roster]
    end

    FS --> W --> P --> G
    TR --> C --> G
    RT --> C
    G <--> DB
    G --> WS
    WS <--> UI
    WS <--> AG
```

**Data flow:** files change → watcher fires → parser extracts AST + doc comments + function signatures + parameter flow → graph patched → patches emitted as CLV events over WebSocket → SvelteFlow renders with doc tooltips. In parallel, test runners, agents, and the runtime tracer print **CLV-tagged lines to stdout**; the collector ingests them, correlates by session / process / agent, and patches node status and hot edges.

### 4.1 Relationship to Conductor (decided: standalone, complementary)

Lattice is a **visualisation and attribution layer** — it watches and reflects, it does not orchestrate. *Conductor* (the separate tmux/Canvas multi-agent orchestration tool) is the **scheduler and agent spawner**.

They are **complementary peers, not merged**:
- Conductor mints the CLV `session` for a run and injects CLV reporting instructions into agents as it spawns them (and may pre-register agents).
- Lattice consumes the resulting CLV streams and renders them.

Any agent workflow — Conductor's, someone else's, or a manual test run — can feed Lattice. Keeping them separate is deliberate.

---

## 5. Backend (Rust)

Rust is chosen over Go specifically because Lattice must be **universal and scale to large codebases** from a single distributable binary.

### 5.1 Crates

| Concern | Crate | Notes |
|---|---|---|
| LSP scaffold | `tower-lsp` | Protocol handshake + async service model |
| Rust AST | `syn` | First-class Rust parsing (Phase 0 language) |
| Generic AST | `tree-sitter` + grammars | Multi-language parsing |
| File watching | `notify` | Cross-platform FS events; debounce bursts |
| Runtime tracing | `tracing` + custom subscriber | Live call stack → hot-edge events |
| Async runtime | `tokio` | Drives watcher, collector, WS, per-process readers |
| WebSocket | `tokio-tungstenite` | Streams CLV events to the client |
| SQLite / Postgres | `sqlx` (both features) | One query layer, two backends |
| Serialisation | `serde` / `serde_json` | CLV event (de)serialisation |
| Git metadata | `git2` | `blame`/`log` for human attribution |

### 5.2 Responsibilities

1. **Watcher** — observe the repo, debounce, emit change events.
2. **Parser layer** — `syn` for Rust, `tree-sitter` otherwise. For each changed file it extracts: structural nodes/edges, **doc comments** (§6.5), and **function signatures + parameter flow** (§6.4). **Recovers from syntax errors** (partial tree, mark node `error`, never panic).
3. **Graph model** — single in-memory source of truth; assigns stable IDs, maintains parent/child links, call edges, and data-flow edges; emits patches.
4. **CLV collector** — read tagged stdout from all processes in parallel, parse, correlate by `session`/`pid`/`agent`, patch node status / hot edges, persist structured events.
5. **Storage layer** — persist behind a trait (see `DATA_MODEL.md`).
6. **WebSocket server** — broadcast events; answer client `snapshot` and `expand` requests; serve docs on demand.

---

## 6. What the parser produces

Full schemas live in `DATA_MODEL.md`. This section describes the *semantics* the parser must implement.

### 6.1 Structural nodes & containment
A node per code element with deterministic ID `type:path:symbol[:child]`, linked by `parentId`/`childIds` (service → module → file → function → variable). Containment is the lazy-loading backbone (§9.1).

### 6.2 Call edges
For each call site, a `kind: calls` edge from caller to callee.

### 6.3 Hot edges (runtime)
Call edges flagged `hot:true` while on the executing stack, sourced from the runtime `tracing` subscriber (high-frequency — throttled, §11.2). Decided **in scope for v1**.

### 6.4 Parameter dependency mapping (static, AST-derived)

When the parser reads a function it extracts:
1. **Parameter names and types** — what it takes in.
2. **Return type** — what it produces.
3. **Call sites within the function** — which functions it invokes and which arguments it passes.
4. **Return-value usage** — where its return value is assigned/consumed downstream.

From this it builds **data-flow edges** (distinct from call edges):
- `kind: param_source` — function A's parameter originates from function B's return value (B *produces* the input A *consumes*).
- `kind: data_flows_from` — function A's return value flows into function C (C *consumes* A's output).

> Example: `authenticate()` returns a token that `verify_token()` consumes → edge `authenticate --data_flows_from--> verify_token`. The graph shows not just *that* functions connect, but *why* (data dependency).

**No runtime value inspection.** This is pure static analysis on the AST — we illustrate *which function depends on which input/output*, not the values flowing through. This keeps it cheap and live.

**Update cadence:** same as the file watcher. On a file change the parser re-extracts signatures and re-derives data-flow edges for that file; changed edges are `edge.upsert`-ed (new `updated_at`) and emitted over WebSocket. Debounced (§11.2) but intended to be **live and continuous**, like the rest of the AST.

### 6.5 Doc-comment extraction (for non-technical stakeholders)

The parser extracts documentation at every level and stores it in the node's `docs` field:
- **Function-level** (`///`, `/** */`, docstrings, etc.).
- **Class / interface-level** — the broader contract.
- **Module-level** — the module's purpose.
- **Variable-level** — important fields/parameters.

Surfaced in the UI as a hover tooltip and a sidebar for the selected node (§9.5). This makes the graph readable by stakeholders, not just engineers. Agents are required to keep these docs current and consistent up the hierarchy — see `AGENT_PROTOCOL.md` §6.

---

## 7. Storage layer (summary)

Relational, **not** vectorised. Two interchangeable backends behind one `sqlx` trait — **SQLite** (solo/local) or **Postgres** (team: Docker container or shared network instance) — selected by `LATTICE_DB_URL`. `process_id` is the correlation thread across tables; it is the primary key on process-scoped tables (`agents`, `protocol_versions`) and a foreign key/attribution column elsewhere. One `session` contains many concurrent `process_id`s, each mapping to one agent. **Raw stdout is ephemeral; only structured CLV events are persisted.**

Full schema, key strategy, and the agent-registry / protocol-version lifecycle are in **`DATA_MODEL.md`**.

---

## 8. CLV stdout tagging protocol (summary)

Tests, agents, and the runtime tracer report state by printing **`#CLV1`-prefixed JSON lines to stdout** — language-agnostic. Event types: `activity`, `test`, `status`, `hotedge`. Correlation nests `session → pid → agent`. The backend reads every process's stdout **in parallel** (a `tokio` task per process); untagged lines pass through to the terminal unchanged.

Full protocol, fields, reference emitters, and agent responsibilities are in **`AGENT_PROTOCOL.md`**.

---

## 9. Frontend (SvelteFlow)

**Why SvelteFlow** (xyflow): same maintainers as React Flow, identical features, but Svelte's reactivity fits a stream of incoming WebSocket patches with less boilerplate.

### 9.1 Hierarchy & lazy loading (critical)
Nodes carry `parentId`/`childIds`; navigation is purely by ID. **Lazy loading is paramount** — the client gets only the top level on connect; expanding a node sends an `expand` request and the backend returns just that subtree. Collapsing discards rendered children to bound memory.

### 9.2 Zoom / drill-down — variable granularity (decided: lazy + zoom-gated)
Infinitely nestable: service → module → file → function → variable. **Variable-level nodes are lazy-loaded and zoom-gated** — at the function level a function renders as a **single node**; variables do **not** render until the user drills into that function, at which point they're requested on demand. A function's internals can **materialise in real time** as code is written, once expanded.

### 9.3 Edge types & filtering
Call edges and data-flow edges (§6.4) coexist; the UI filters/colours by `kind` so users can toggle between control flow and data flow. Hot edges animate while active.

### 9.4 Agent layer & live roster
A toggleable view where **agents are top-level nodes**. Click an agent → the code it touched (`authored_by` edges) → drill into the implementation. **Bidirectional:** click a code node → which agents touched it. Backed by the `agents` table, so it shows a **live roster** (active vs inactive, colour-coded by `agent_type`, respawn reflected via new `process_id`s).

### 9.5 Documentation surfacing
Hovering a node shows its extracted `docs` (§6.5) as a tooltip; selecting a node opens a sidebar with the full description. Works at every zoom level so stakeholders see accurate, human-readable descriptions of services, modules, classes, and functions.

### 9.6 Visual language & UI kit
- **UI kit: `shadcn-svelte`** — lightweight, copy-paste components on Tailwind, deep customisation.
- Dark + light mode (SvelteFlow first-class theming).
- Nodes colour-coded by `type`; agents by `agent_type`.
- Test status via border/background + badge: green = passing, red = failing, pulsing = running, grey = stale, hatched = error.

---

## 10. Repo loading & lifecycle

User points Lattice at a local directory (file picker in the UI or a CLI flag). The backend immediately starts the watcher, runs the initial parse, builds the graph, and opens the WebSocket. **Fully automated thereafter** — no per-run setup. Users run `cargo test` / `npm test` / `pytest` as usual; CLV output is detected, correlated, and reflected automatically. Lattice stays listening in the background.

---

## 11. Non-functional requirements

### 11.1 Resilience / graceful degradation
- **Backend crash:** persisted DB state allows the graph to be rebuilt on restart; the frontend shows a clear "reconnecting" state.
- **WebSocket drop:** client auto-reconnects with backoff and requests a fresh `snapshot` to **resync** rather than trusting stale local state.
- **Malformed code:** parser **recovers** — partial tree, offending node marked `error`, the rest stays live. Never crash on bad syntax.

### 11.2 Performance
- Debounce rapid file-change bursts before re-parsing (applies to AST, docs, and data-flow re-derivation).
- Lazy, zoom-gated subtree loading is the primary scalability lever (§9.1–9.2).
- **Hot edges are high-frequency** — the tracing subscriber must throttle/sample (coalesce rapid enter/exit, or use a dedicated binary channel rather than line-based stdout) so call-path tracing doesn't flood the collector on hot loops.
- Bounded client memory: collapsed subtrees are not retained.

### 11.3 Self-observability
Expose tool metrics (parse latency per file, node/edge counts, memory footprint, events/sec) — optionally a debug panel — so users can see how Lattice copes on large codebases.

### 11.4 Privacy
Local-first. No telemetry by default. Code never leaves the machine unless the user configures a remote Postgres instance.

---

## 12. Distribution

**Cross-platform single binary** via Rust, published on **GitHub Releases** with pre-built artifacts for **Windows, macOS, and Linux**. Download, run, point at a repo. `cargo install` secondary. Optional later: Homebrew / Chocolatey / distro packages, and an npm wrapper for JS-ecosystem discoverability.

---

## 13. Decisions log

Resolved during design (rationale kept for the builder):

1. **Lattice is standalone**, complementary to Conductor (§4.1).
2. **JSON over WebSocket**, not XML (cheap diff/patch; SvelteFlow fit).
3. **SvelteFlow over React Flow** (reactivity fits streamed patches).
4. **Rust over Go** (universal scale, single binary).
5. **Variable nodes lazy-loaded + zoom-gated** (§9.2).
6. **Hot-edge tracing in scope for v1**, throttled (§6.3, §11.2).
7. **Parameter dependency mapping is static/AST-derived** — no runtime value tracking (§6.4).
8. **Doc comments extracted and surfaced** for non-technical stakeholders (§6.5, §9.5).
9. **Agents must keep docs current and cascade changes up the hierarchy** (`AGENT_PROTOCOL.md` §6).
10. **`process_id` is the unifying thread** across tables (`DATA_MODEL.md`).
11. **One session → many processes → one agent each** (`DATA_MODEL.md`).
12. **Raw stdout ephemeral; only structured CLV events persisted** (`DATA_MODEL.md`).
13. **Agent metadata + protocol version stored in the DB**, pinned per process (`DATA_MODEL.md`).

**No open blockers.** Validate during build: hot-edge transport (stdout vs binary channel) under real load; whether per-process `protocol_versions` should be normalised once usage is known.
