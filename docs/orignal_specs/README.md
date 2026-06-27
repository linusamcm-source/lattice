# Lattice

> A **live code & agent visualiser** — renders an interactive, real-time map of a codebase as it is written and tested.

Lattice watches a repository continuously. As code changes and tests run, it shows you a drill-downable graph: structure (service → module → file → function → variable), live test status (TDD red/green/running), live call paths, **parameter data-flow between functions**, and — in multi-agent workflows — **which agent touched what**, with a live roster. Doc comments are extracted and surfaced so the graph is legible to non-technical stakeholders too.

It is **local-first**, **platform-agnostic** (any language), and ships as a **single cross-platform binary**.

---

## Documentation set

Read in this order. Each doc owns a distinct concern; cross-references point to the owner rather than duplicating.

| Doc | Owns |
|---|---|
| **README.md** (this file) | Overview, glossary, tech stack summary, doc index. |
| **SPEC.md** | Architecture, components, behaviour, frontend, non-functional requirements. The "what and why." |
| **DATA_MODEL.md** | The contracts: JSON wire schema (WebSocket) + SQL database schema. The single source of truth everything else references. |
| **AGENT_PROTOCOL.md** | The CLV stdout tagging protocol and the responsibilities of agents that feed Lattice (including the documentation-maintenance requirement). |
| **BUILD_PLAN.md** | Phased implementation order with per-phase acceptance criteria. The build runbook for an implementing agent. |

---

## What makes Lattice different

Existing tools split the problem and never rejoin it: dependency-graph viewers show structure but not live test state or data flow; live test runners show pass/fail but no architectural map; coverage dashboards are CI-oriented and post-hoc; none extract documentation for non-technical viewers. Lattice combines all of it in one real-time, drill-downable view.

---

## Tech stack at a glance

- **Backend:** Rust — `tower-lsp`, `syn` (Rust AST), `tree-sitter` (generic AST), `notify` (file watch), `tracing` (runtime call stack), `tokio`, `tokio-tungstenite` (WebSocket), `sqlx` (SQLite + Postgres), `serde`/`serde_json`, `git2`.
- **Transport:** JSON over WebSocket.
- **Storage:** SQLite (local) or Postgres (team) — selected by connection string. `process_id` is the correlation thread across tables.
- **Frontend:** SvelteFlow (xyflow) + `shadcn-svelte` on Tailwind; dark/light mode.
- **Distribution:** single binary via GitHub Releases (Windows / macOS / Linux); `cargo install` secondary.

---

## Glossary

- **CLV** — *Code-Live-View*, the stdout tagging protocol (`#CLV1` sentinel) agents and runners use to report state. Defined in `AGENT_PROTOCOL.md`.
- **Node** — a code element (service/module/file/function/variable) or an agent. Identity is deterministic: `type:path:symbol`.
- **Call edge** — control-flow dependency (`kind: calls`).
- **Parameter / data-flow edge** — where a function's input comes from or its output goes (`kind: param_source | data_flows_from`). Static, AST-derived; no runtime value tracking.
- **Hot edge** — a call edge currently on the executing stack (`hot: true`), sourced from runtime tracing.
- **Session** — one orchestration run. Contains many concurrent `process_id`s; each `process_id` maps to exactly one agent.
- **Agent** — a contributor (e.g. `tdd-red`, `security-scanner`) or a human. First-class node, tracked in a live roster.
- **Conductor** — a *separate* multi-agent orchestration tool. Lattice is the visualiser; Conductor is the scheduler. They are complementary, not merged (see SPEC §4.1).
