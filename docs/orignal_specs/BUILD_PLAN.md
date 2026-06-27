# Lattice — Build Plan

> Phased implementation order for an implementing agent. Each phase is independently runnable and has acceptance criteria. Build in order; later phases depend on earlier ones. Schemas: `DATA_MODEL.md`. Behaviour/rationale: `SPEC.md`. Agent rules: `AGENT_PROTOCOL.md`.

---

## Principles

- **Walking skeleton first**, then enrich. Get an end-to-end path live before adding depth.
- **Happy path before robustness.** Resilience/perf hardening is a dedicated late phase, but never *crash* — parser error recovery (partial tree, mark `error`) is in from the moment parsing exists.
- **Contracts are fixed** by `DATA_MODEL.md`. Don't invent schema; extend it deliberately if a gap appears and note it.

---

## Phases

### Phase 0 — Walking skeleton
**Scope:** `notify` watcher → `syn` (Rust only) → in-memory graph → `tokio-tungstenite` WebSocket → SvelteFlow rendering **root level only**. SQLite hardcoded. No tests, agents, docs, or params yet.
**Accept when:** editing a Rust file updates a node's label live in the browser.

### Phase 1 — Hierarchy + lazy loading (+ variable zoom-gating)
**Scope:** `parentId`/`childIds`; expand/collapse; client `expand` request → backend subtree reply; **variable nodes lazy-loaded and zoom-gated** (hidden at function level).
**Accept when:** drilling into a function reveals its variables on demand; collapsing frees them; the canvas stays responsive with a deep tree.

### Phase 2 — Multi-language
**Scope:** `tree-sitter` parser path for ≥2 more languages (e.g. Python, TypeScript) behind the same node/edge model.
**Accept when:** the same hierarchy + lazy-load behaviour works on Python and TS files.

### Phase 3 — Doc-comment extraction & surfacing
**Scope:** parser extracts function/class/module/variable docs into `node.docs`; frontend shows docs on hover + a sidebar for the selected node (`SPEC.md` §6.5, §9.5).
**Accept when:** hovering a documented function shows its description at every zoom level; updating the source updates the shown doc.

### Phase 4 — Parameter dependency mapping
**Scope:** parser extracts function signatures (params/returns), call-site arguments, and return-value usage; builds `param_source` / `data_flows_from` edges; frontend filters/colours call vs data-flow edges (`SPEC.md` §6.4).
**Accept when:** a function consuming another's return value shows a data-flow edge; editing the dependency re-derives the edge on save; control-flow and data-flow can be toggled independently.

### Phase 5 — Test tracking (CLV collector)
**Scope:** CLV stdout collector with **parallel per-process readers**; `#CLV1` parsing; `session`/`pid`/`agent` correlation; `status.update` / `test.result` events; node colouring; untagged lines pass through.
**Accept when:** a failing test reddens its node within ~1s; a parallel run in another terminal never contaminates it (distinct `session`/`pid`).

### Phase 6 — Live call-path (hot edges)
**Scope:** runtime `tracing` subscriber emits `hotedge` enter/exit; backend toggles `edge.hot`; frontend animates; **throttling/sampling** so hot loops don't flood (`SPEC.md` §11.2).
**Accept when:** running code lights its call path in real time and clears on exit, without overwhelming the collector under a hot loop.

### Phase 7 — Storage abstraction
**Scope:** storage trait; SQLite + Postgres via `sqlx`; `LATTICE_DB_URL` config; full schema incl. `sessions`, `nodes`, `edges`, `test_results`, `agent_activity`, `protocol_versions`; `process_id` keying per `DATA_MODEL.md`; **raw stdout not persisted**.
**Accept when:** the same run persists identically to local SQLite and a Docker Postgres with no code change; only structured CLV events are written.

### Phase 8 — Agent layer + registry
**Scope:** `agents` table (pid PK) with active/inactive + respawn lifecycle; `authored_by` edges; agent-as-node layer with bidirectional drill-down; live roster (`agent.roster` events); per-process protocol-version pinning.
**Accept when:** clicking an agent shows its touched nodes and live status; clicking a node shows its agents; respawning an agent type shows a new `pid` seamlessly.

### Phase 9 — Resilience + performance
**Scope:** WebSocket reconnect with `snapshot` resync; backend-crash rebuild from DB; debounce file bursts; hot-edge throttling; bounded client memory; self-observability metrics panel (parse latency, node/edge counts, memory, events/sec).
**Accept when:** killing the backend and feeding malformed code both degrade gracefully; the client resyncs cleanly after a forced disconnect.

### Phase 10 — Packaging
**Scope:** CI builds cross-platform binaries → **GitHub Releases** (Windows / macOS / Linux); `cargo install` path.
**Accept when:** a fresh machine on each OS can download-and-run, point at a repo, and see a live graph.

---

## Cross-cutting acceptance criteria

- Editing source produces a JSON node/edge patch matching `DATA_MODEL.md` within debounce budget.
- A concurrent second test run (different terminal) never contaminates the first run's statuses.
- Reconnecting after a forced disconnect yields a graph identical to the server's state.
- A syntax error marks exactly the affected node `error` and leaves siblings live.
- Switching `LATTICE_DB_URL` from SQLite to Postgres requires no code change.
- An agent emitting CLV `activity` yields a visible `authored_by` edge and an `active` roster entry; on exit it flips to `inactive`.
- Function nodes render without their variables until expanded.
- A running call path lights the correct edges `hot` and clears them on exit, under throttling.
- A documented element shows accurate docs at every zoom level; a parameter dependency shows the correct data-flow edge.

---

## Dependency notes for the builder

- Phases **3 and 4 are both parser enrichment** and can be developed in parallel once Phase 2 lands, but keep doc extraction (3) ahead of or alongside param mapping (4) since both touch the same signature-extraction pass.
- Phase **6 (hot edges)** is the main performance risk — prototype the transport (line-based stdout vs a dedicated binary channel) early and measure under a hot loop before committing.
- Phase **8 (agent layer)** depends on Phase 5's correlation and Phase 7's `agents` table.
- The `protocol_versions` per-process model (Phase 7) may be normalised later if many processes share a version — see `DATA_MODEL.md` §B.6 builder note.
