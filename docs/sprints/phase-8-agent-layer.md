# Lattice Phase 8 — Agent layer + registry

Phase 8 turns the **agent attribution** that already flows through CLV into a live, queryable
layer: agents become first-class nodes, the code they touch is linked by `authored_by` edges, a
real-time roster shows who is active/inactive, and each process is pinned to its CLV protocol
version. This is the BUILD_PLAN.md Phase 8 deliverable and it sits on top of Phase 5 (CLV
correlation), Phase 7 (storage + the `agents`/`agent_activity`/`protocol_versions` tables), and
the wire scaffolding that earlier phases reserved but never wired.

**System-level "done" (BUILD_PLAN.md §Phase 8 accept):** clicking an agent shows its touched
nodes and live status; clicking a code node shows its agents; respawning an agent type shows a new
`pid` seamlessly. Contracts are fixed by `docs/orignal_specs/DATA_MODEL.md` §A.5 (payloads), §B.3
(registry lifecycle), §B.4 (protocol version), §B.6 (DDL); behaviour by `SPEC.md` §9.4; the CLV
rules by `AGENT_PROTOCOL.md` §4–§5.

## Grounding — what already exists (verified by recon this session)

The scaffolding is unusually complete; Phase 8 is mostly *wiring inert reservations to live data*,
not new contract design. Verified facts (file:line):

- **Wire tags already declared, payloads absent.** `crates/backend/src/wire.rs:180-188` declares
  `EventType::AgentActivity` (`"agent.activity"`) and `EventType::AgentRoster` (`"agent.roster"`);
  `wire.rs:146-147` declares `EdgeKind::AuthoredBy` (`"authored_by"`); `wire.rs:57-58,77` declares
  `NodeType::Agent` (serde `"agent"`, `id_prefix "agent"`). The `#[serde(untagged)] Payload` enum
  (`wire.rs:316-454`) has **no** agent variant — its last arm is `HotEdge` (`wire.rs:434`). Outside
  wire.rs's own tests these three tags are never constructed.
- **Activity is parsed but dropped.** `ClvEvent::Activity { session, pid, agent, msg, node, action }`
  is parsed today (`crates/backend/src/clv.rs:35-48`), but `Graph::apply_clv` no-ops it:
  `ClvEvent::Activity { .. } => None` at `crates/backend/src/graph.rs:317`. The `Test`/`Status`/
  `HotEdge` arms (`graph.rs:271-358`) each return a single `Option<EventEnvelope>`.
- **Storage tables exist, two are never written.** `crates/backend/src/storage/sqlite.rs:63-142`
  creates `agents` (process_id PK), `protocol_versions` (process_id PK), `agent_activity` (id PK).
  `upsert_agent` (`sqlite.rs:411-434`) hardcodes `agent_type = agent_id`, `color = "#888888"`,
  `status = 'active'` and only fires when an event carries a `pid`. There is **no** `INSERT INTO
  agent_activity` or `INTO protocol_versions` anywhere (`sqlite.rs:34-36` documents this; tests at
  `sqlite.rs:926-947` assert both stay empty). `persist` dispatches over `&env.payload`
  (`sqlite.rs:208-345`). Postgres (`storage/postgres.rs`) is the structural twin and mirrors the gap.
- **All writes funnel through one trait method.** `Storage` (`storage/mod.rs:135-154`) is exactly
  `ensure_schema`, `persist(env)`, `record_session`. Adding `Payload` arms to `persist` is the
  established seam — no new trait methods required.
- **Persistence auto-flows.** A subscriber on the broadcast channel (`app.rs:217-236`) calls
  `store.persist(&env)` for every `EventEnvelope`; any new envelope `apply_clv` emits is persisted
  with no extra wiring.
- **Collector is a single sink-tail, not per-process.** `collect`/`poll_once`
  (`collector.rs:82-150`) tail one append-only file `<root>/.lattice/clv.ndjson`, forwarding each
  parsed line to `graph.apply_clv` and broadcasting the returned envelope (`collector.rs:143-147`).
  There is **no** per-pid stream and **no** EOF/process-exit signal. The collector holds no
  per-session state (`collector.rs:39-43`). Therefore roster `inactive` cannot key off real process
  exit — it must be an **idle-timeout** per `(session, pid, agent)`.
- **Frontend mirror is partly ready.** `frontend/src/lib/types.ts:15` already has `'agent'` in
  `NodeType` and `:26` already has `'authored_by'` in `EdgeKind`. The `EventEnvelope` union
  (`types.ts:189-198`) has **no** agent arm; `KNOWN_EVENT_TYPES` (`ws.ts:129-139`) gates parsing so
  an unknown type is dropped at `ws.ts:188`; `GraphState` (`ws.ts:31-34`) is `{nodes, edges}` Maps
  with no roster; `applyEvent` (`ws.ts:63-127`) is an exhaustive `switch` with no `default`. Node
  styling is keyed by **status only** (`layout.ts:110-117`); `HierarchyNodeData` (`layout.ts:65-89`)
  never carries `node.type`. The edge-filter `<fieldset>` (`Graph.svelte:155-169`, booleans at
  `:74-76`) and `Sidebar.svelte` (a `w-72 shrink-0` aside) are the in-repo patterns to mirror for the
  layer toggle and the roster panel. **No shadcn-svelte / component library is installed** — UI is
  hand-rolled Tailwind over `@xyflow/svelte` only.

## Design decisions (deliberate, grounded — declared so Phase 1 review sees them as intentional)

1. **Agent node identity = `agent:<agentId>` (one node per stable agent id), roster row = per
   `process_id`.** `AGENT_PROTOCOL.md §4` says a dormant colour/role re-activates by spawning a new
   process under the **same `agent` id**, and `§3` maps one `agentId` to one `agent_type`/`color`.
   So the *visual* agent node is keyed by the stable `agentId` and persists across respawns, while
   the DB `agents` table stays keyed by `process_id` (`DATA_MODEL.md §B.3`, `sqlite.rs:70-79`) and
   the `agent.roster` payload lists **per-process** entries. An agentId is "active" iff any of its
   processes is active. This satisfies "respawn shows a new `pid` seamlessly" (new row, same node).
2. **`Graph` owns the in-memory roster.** The shared `Arc<Mutex<Graph>>` (`app.rs:195`) is already
   the collector's mutation target; roster state (a `HashMap` keyed by `process_id`) lives there so
   it survives across `poll_once` calls and is reachable for snapshot/idle-timeout. No new shared
   state in `app.rs`.
3. **`apply_clv` returns `Vec<EventEnvelope>`.** Activity must emit up to four envelopes
   (agent `node.upsert`, `authored_by` `edge.upsert`, `agent.activity`, `agent.roster`). The current
   `Option<EventEnvelope>` return (`graph.rs:269`) is widened to `Vec`; the single caller
   (`collector.rs:143-147`) is updated to iterate. Test/Status/HotEdge arms return one-element vecs.
4. **`inactive` is an idle-timeout, not a process signal** (see grounding). Default idle window is a
   named constant; respawn is a brand-new `process_id` so it needs no special-casing.

---

## Story P8-1: Agent wire payloads + agent-node id convention (`wire.rs`)

Add the two missing `Payload` variants and a stable agent-node id helper to the Rust wire contract
so later stories have types to emit, persist, and mirror. Pure contract + helpers; no behaviour
wiring. Builds on the verified facts that `EventType::AgentActivity`/`AgentRoster`
(`wire.rs:180-188`), `EdgeKind::AuthoredBy` (`wire.rs:146-147`) and `NodeType::Agent`
(`wire.rs:57-58`) already exist, and that `Payload` is `#[serde(untagged)]` with **order-sensitive**
decoding (`wire.rs:300-315`). Field names must match `DATA_MODEL.md §A.5` exactly (camelCase via
serde rename, as the existing payloads do).

### Depends On: none
### Touches: crates/backend/src/wire.rs, crates/backend/src/storage/sqlite.rs, crates/backend/src/storage/postgres.rs, docs/orignal_specs/DATA_MODEL.md

### Acceptance Criteria
- A `Payload::AgentActivity` variant exists carrying (serde-camelCase) `agentId: String`,
  `action: String`, `nodeId: String`, `sessionId: String`, `processId: Option<u32>`, and optional
  `ts`/`msg` (every optional `#[serde(skip_serializing_if = "Option::is_none")]`); with the optionals
  `None` it serializes **byte-identically** to `DATA_MODEL.md §A.5` `agent.activity`, and is a
  documented superset when they are set. `processId` is `u32` to match every existing wire payload and
  `ClvEvent.pid` (`wire.rs:397/422/444`, `clv.rs:39`) — no `u64` widening.
- A `Payload::AgentRoster` variant exists carrying `sessionId: String` and `agents: Vec<AgentInfo>`,
  where `AgentInfo` has `processId: u32`, `agentId: String`, `agentType: String`, `color: String`,
  `status: String` (`active`|`inactive`), and optional `protocolVersion: String`
  (`#[serde(skip_serializing_if = "Option::is_none")]`); with `protocolVersion` `None` it serializes
  byte-identically to `DATA_MODEL.md §A.5` `agent.roster`, and is a documented superset when set.
- **The workspace still compiles and `just qg` is green after adding the variants.** The `persist`
  match in `storage/sqlite.rs` (`:339`) and `storage/postgres.rs` (`:422`) is exhaustive with **no**
  `_ =>` wildcard, so the two new `Payload` variants would be E0004 non-exhaustive. P8-1 therefore
  adds a temporary no-op catch-all arm `Payload::AgentActivity { .. } | Payload::AgentRoster { .. } =>
  {}` to **both** matches — an empty block yielding `()`, identical in form to the existing
  `Payload::Snapshot { .. } | Payload::Subtree { .. } => { … }` no-op arm, so the arm type matches its
  siblings and the trailing `tx.commit().await?; Ok(())` (it is **not** `=> Ok(())`, which is an E0308
  "incompatible match arms" error). A unit test asserts persisting an agent envelope is a no-op for
  now; P8-3 replaces these stubs with the real writes.
- A pure helper (e.g. `agent_node_id(agent_id: &str) -> String`) returns `"agent:<agentId>"` and is
  deterministic for the same input (asserted by a unit test).
- `serde_json` round-trips both new payloads (serialize → deserialize → equal); a unit test feeds the
  literal JSON from `DATA_MODEL.md §A.5` and decodes it into the right `Payload` variant.
- Untagged-decode disambiguation is proven: a unit test confirms an `agent.roster` JSON object does
  **not** mis-decode as any earlier `Payload` variant and vice-versa (each new variant carries a
  required disambiguator field, ordered correctly within the enum per `wire.rs:300-315`).
- `protocol_sentinel()` / `PROTOCOL_VERSION` are unchanged (the CLV sentinel stays `#CLV1`).

### Definition of Done
- New `#[cfg(test)]` cases in `wire.rs` cover round-trip, literal-JSON decode, untagged
  disambiguation, and id determinism; `just test` green; new-code coverage ≥ 90%.
- `just qg` (fmt-check + lint + test) clean — clippy `-D warnings`.
- The transient `postgres.rs` no-op stub arm is coverage-excluded for the new-code gate when the
  sprint env has no Docker Postgres (same carve-out as P8-3); the `sqlite.rs` stub arm is exercised by
  the no-op persist unit test.
- Doc comments on both new `Payload` variants and `AgentInfo` and the id helper, per
  `AGENT_PROTOCOL.md §6`; if any field deviates from `DATA_MODEL.md §A.5`, `DATA_MODEL.md` is updated
  in the same story with a noted, deliberate extension (otherwise it is left untouched).

## Story P8-2: Activity → agent node + `authored_by` edge + in-memory roster (`graph.rs`)

Replace the `ClvEvent::Activity { .. } => None` no-op at `graph.rs:317` with real attribution. Add a
`process_id`-keyed roster map to the `Graph` struct (`graph.rs:55-66`); on an activity event upsert
an `agent:<agentId>` node (`NodeType::Agent`), upsert an `authored_by` edge from the touched code
node to that agent node, mark the process active in the roster, and emit the envelopes. Widen
`apply_clv` (`graph.rs:269`) to return `Vec<EventEnvelope>` and update its sole caller in
`poll_once` (`collector.rs:143-147`) to iterate-and-broadcast. Ensure agent nodes + `authored_by`
edges surface to clients (agent nodes are roots, so `snapshot` at `graph.rs:108-130` carries them;
`subtree` filters to `Contains`-only at `graph.rs:152`, so attribution rides explicit upserts/
snapshot, not subtree).

### Depends On: P8-1
### Touches: crates/backend/src/graph.rs, crates/backend/src/collector.rs

### Acceptance Criteria
- Feeding a parsed `ClvEvent::Activity` (agent `tdd-green`, pid 48213, node
  `fn:src/x.rs:foo`, action `modified`) into `Graph::apply_clv` returns a `Vec` containing: a
  `node.upsert` for node id `agent:tdd-green` of type `agent`, an `edge.upsert` of kind
  `authored_by` from `fn:src/x.rs:foo` to `agent:tdd-green`, an `agent.activity` envelope, and an
  `agent.roster` envelope.
- The returned `agent.roster` lists the process as `status: "active"` with its `agentId`/`processId`.
- A second activity from the **same** `agentId` but a **new** `pid` yields a roster with two
  process rows under one `agentId` and reuses the same `agent:<agentId>` node (no duplicate node
  upsert id); the `authored_by` edge id is stable/deterministic.
- `apply_clv` for `Test`/`Status`/`HotEdge` still produces the same single envelope as before (now a
  one-element `Vec`) — existing behaviour unchanged (regression-asserted).
- `poll_once` broadcasts **every** envelope returned by `apply_clv` (a multi-envelope activity is
  fully fanned out, verified via the broadcast receiver in a collector test).
- After an activity, the agent node (a root, `parentId == null`) and the `authored_by` edge reach
  clients via the emitted `node.upsert` / `edge.upsert` envelopes. A fresh-connect `snapshot`
  includes the agent **node** (it is a root) but **not** the `authored_by` edge — `snapshot` emits an
  edge only when both endpoints are roots (`graph.rs:121-128`) and the edge's code-node source is a
  non-root function; this preserves the existing root-only snapshot invariant (attribution rides
  explicit upserts, not snapshot/subtree, per this plan's grounding).

### Definition of Done
- New `#[cfg(test)]` cases in `graph.rs` (apply_clv multi-envelope, node/edge identity, respawn
  reuse, snapshot inclusion, Test/Status/HotEdge regression) and `collector.rs` (fan-out);
  `just test` green; new-code coverage ≥ 90%.
- `just qg` clean.
- Doc-comment cascade: the new roster field on `Graph` documented; the `apply_clv` doc updated to
  describe the activity path and `Vec` return; the `graph.rs` module doc (`//!`) re-checked so the
  "agent layer" responsibility is accurate at the module level (`AGENT_PROTOCOL.md §6`).

## Story P8-3: Persist the agent layer — `agents`, `agent_activity`, `protocol_versions` (storage)

Make the three reserved tables hold real data. Add `persist` arms for `Payload::AgentActivity`
(`INSERT INTO agent_activity`, table at `sqlite.rs:125-133`) and `Payload::AgentRoster` (upsert real
`agents` rows with the roster's true `agent_type`/`color`/`status`, and a `protocol_versions` row
per process, tables at `sqlite.rs:70-87`). Replace the `upsert_agent` placeholders
(`sqlite.rs:411-434`: `agent_type = agent_id`, `color = "#888888"`, hardcoded `'active'`) with
roster-supplied values, honouring the `process_id` PK + `ON CONFLICT` lifecycle from
`DATA_MODEL.md §B.3`. Mirror every change in `postgres.rs` (the `$N`/`BIGINT` twin). `persist` stays
the single seam (`storage/mod.rs:135-154` unchanged).

### Depends On: P8-1
### Touches: crates/backend/src/storage/sqlite.rs, crates/backend/src/storage/postgres.rs

### Acceptance Criteria
- Persisting an `agent.roster` envelope writes one `agents` row per `AgentInfo` with the **real**
  `agent_type`, `color`, and `status` from the payload (not the `"#888888"`/`agent_id` placeholders),
  keyed by `process_id`; a re-emitted roster flipping a process to `inactive` updates that row's
  `status` and `updated_at` via `ON CONFLICT(process_id)` without inserting a duplicate.
- Persisting an `agent.roster` writes/refreshes one `protocol_versions` row per process (`version`
  derived from the CLV sentinel, e.g. `"1"`), keyed by `process_id`, FK-valid against `agents`.
- Persisting an `agent.activity` envelope writes one `agent_activity` row (`agent_id`, `action`,
  `node_id`, `process_id`, `session_id`, `ts`); the row count increments by exactly one per event.
  The arm first `upsert_session` + (when a `pid` is present) `upsert_agent` **before** the
  `INSERT INTO agent_activity`, mirroring the existing `TestResult`/`StatusUpdate`/`HotEdge` arms
  (`sqlite.rs:226`), so the `agent_activity.process_id REFERENCES agents(process_id)` FK holds
  regardless of envelope ordering (the `agents` row is otherwise created only by `agent.roster`); a
  test asserts an activity-then-query with no prior roster still writes the row (no FK violation).
- The Phase-7 tests that asserted `agent_activity`/`protocol_versions` stay empty
  (`sqlite.rs:926-947`) are updated to assert the new write behaviour (the "stay empty" invariant is
  intentionally retired for these two tables, documented in the test).
- The SQLite and Postgres schemas remain identical in shape; switching `LATTICE_DB_URL` requires no
  code change (the Docker-gated Postgres parity test from Phase 7 is extended to cover an agent
  roster + activity round-trip, skipped when no Postgres is available). The new `postgres.rs` persist
  arms are **structural twins** of the `sqlite.rs` arms; the new-code coverage gate is met on the
  always-run `sqlite.rs` arms, and `postgres.rs` is coverage-excluded for the gate when the sprint
  env has no Docker Postgres — matching the Phase-7 precedent — with parity asserted by the gated
  round-trip test under `LATTICE_TEST_PG` when Docker is present.

### Definition of Done
- New/updated `#[cfg(test)]` cases in `sqlite.rs` (roster upsert real values, inactive flip,
  protocol_versions write, agent_activity write) and the gated postgres parity test; `just test`
  green; new-code coverage ≥ 90%.
- `just qg` clean.
- Module docs in `sqlite.rs`/`postgres.rs` updated to remove the "never written in Phase 7" note for
  these tables and describe the Phase-8 write paths (`AGENT_PROTOCOL.md §6`).

## Story P8-4: Roster lifecycle — idle-timeout `inactive` + respawn (`collector.rs`, `graph.rs`)

Give the roster a real lifecycle. Because the collector has no process-exit signal
(`collector.rs:82-150`), add an **idle-timeout**: track `last_seen` per `process_id` in the
`Graph` roster (from P8-2) and, driven from the **`collect` loop itself on every tick**
(`collector.rs:90-93`) — **not** inside `poll_once`, which returns early on a quiet sink via
`if len == *offset { return }` (`collector.rs:120-127`) and so would never expire idle processes when
no new lines arrive — flip any process whose last activity is older than a named idle window to
`status: inactive`, emitting an `agent.roster` envelope when any status changes. Respawn needs no
special handling — a new `pid` is a fresh active row under the same `agentId` node (from P8-2).

### Depends On: P8-2
### Touches: crates/backend/src/collector.rs, crates/backend/src/graph.rs

### Acceptance Criteria
- A named idle-window constant exists; a `Graph` method (e.g. `expire_idle(now)`) flips every
  process whose `last_seen` is older than the window to `inactive` and returns an `agent.roster`
  envelope **only when** at least one status actually changed (no envelope when nothing changes).
- Given two active processes where one is idle past the window and one is fresh, `expire_idle`
  returns a roster marking exactly the idle one `inactive` and leaves the fresh one `active`.
- After a process is marked `inactive`, a new activity from a **new** `pid` under the same `agentId`
  yields a roster where the old pid stays `inactive` and the new pid is `active`, both under one
  `agentId` (respawn, deterministic against a controllable `now`).
- The collector invokes `expire_idle` from the `collect` loop on every tick **independent of file
  growth** (not behind `poll_once`'s no-growth early return), and broadcasts the resulting roster
  envelope (verified through the broadcast receiver with an injected/controllable clock so the test
  is not wall-clock-flaky). A test with a quiet sink (no new lines) confirms an idle process is still
  expired.

### Definition of Done
- New `#[cfg(test)]` cases (idle flip, partial flip, respawn-after-inactive, no-op when unchanged)
  using a deterministic time input — **no real sleeps** in tests; `just test` green; new-code
  coverage ≥ 90%.
- `just qg` clean.
- Doc comments on the idle-window constant and `expire_idle`; collector module doc updated to note it
  now drives roster expiry (`AGENT_PROTOCOL.md §6`).

## Story P8-5: Frontend ingest — roster state + reducer (`types.ts`, `ws.ts`)

Mirror the P8-1 wire contract on the TS side and fold roster events into `GraphState`. Add
`agent.roster`/`agent.activity` arms to the `EventEnvelope` union (`types.ts:189-198`) with matching
`AgentRosterPayload`/`AgentActivityPayload`/`AgentInfo` interfaces (field names identical to the
serde-camelCase Rust payloads); add both strings to `KNOWN_EVENT_TYPES` (`ws.ts:129-139`) so
`parseEnvelope` (`ws.ts:188`) stops dropping them and add their shape checks to `isValidPayload`
(`ws.ts:151-165`); add an `agents: Map<string, AgentInfo>` to `GraphState` (`ws.ts:31-34`) seeded by
`initialState` (`ws.ts:37-39`); add `applyEvent` branches (`ws.ts:63-127`, after `hot_edge` at
`:125`) — `agent.roster` rebuilds/merges the roster map keyed by `processId`, `agent.activity`
optionally refreshes node attribution; expose a derived `agents` store beside `nodes`/`edges`
(`ws.ts:201-204`). Adding the union arms forces the exhaustive `switch` to be updated (compile-time
gate — no `default`).

### Depends On: P8-1
### Touches: frontend/src/lib/types.ts, frontend/src/lib/ws.ts, frontend/src/lib/ws.test.ts

### Acceptance Criteria
- `parseEnvelope` accepts a well-formed `agent.roster` and `agent.activity` message (matching the
  P8-1 JSON) and returns a typed envelope; a malformed payload (missing `agents`/`agentId`) returns
  `null` (never throws, never widens to `any`).
- `applyEvent` folding an `agent.roster` populates `GraphState.agents` keyed by `processId` with the
  payload's `agentType`/`color`/`status`; a later roster with a process flipped to `inactive` updates
  that entry; the reducer stays pure (returns fresh Maps, input unchanged).
- The derived `agents` store emits the current roster after ingest; `nodes`/`edges` continue to carry
  the agent `node.upsert` and `authored_by` `edge.upsert` that ride the existing channels.
- `EventType` (derived `EventEnvelope['type']`) now includes the two agent types and `npm run check`
  is clean (the exhaustive `switch` compiles only because the branches were added).

### Definition of Done
- New vitest cases in `ws.test.ts` (parse accept/reject, roster fold, inactive update, purity,
  derived store) ; `npm test` green; new-code coverage ≥ 90%.
- `npm run check` (svelte-check strict) zero errors; `npm run lint` (prettier) clean.
- TSDoc on the new payload/`AgentInfo` interfaces and the new reducer branches; the `ws.ts` module
  doc re-checked so the "agent layer" reducer responsibility reads accurately.

## Story P8-6: Agent-layer view — roster panel, type styling, bidirectional drill-down, toggle (UI)

Render the agent layer. Add an **agent-layer toggle** checkbox to the existing edge `<fieldset>`
(`Graph.svelte:155-169`, following the `controlFlow`/`dataFlow` `$state` pattern at `:74-76`); thread
`type: node.type` into `HierarchyNodeData` (`layout.ts:65-89`, populated in `buildHierarchy`
`:162-169`) and add agent-node styling (a type→class map sibling to `STATUS_NODE_CLASS`
`layout.ts:110-117`, branched in `HierarchyNode.svelte:32-36`); extend `flowClassOf`/`buildEdges`
(`layout.ts:227-238`, `275-307`) so `authored_by` edges draw only under the agent toggle; add a new
`RosterPanel.svelte` (mirroring `Sidebar.svelte`'s `w-72 shrink-0` aside) that lists the live roster
from the P8-5 `agents` store, colour-coded by `agentType` with an active/inactive indicator, mounted
beside `<Sidebar>` (`Graph.svelte:172`). Wire **bidirectional drill-down**: click an agent in the
roster → highlight the code nodes it authored (resolve `authored_by` edges); click a code node
(existing `onSelect` path, `HierarchyNode.svelte:41`) → list the agents that touched it.

### Depends On: P8-5, P8-2, P8-4
### Touches: frontend/src/lib/Graph.svelte, frontend/src/lib/HierarchyNode.svelte, frontend/src/lib/layout.ts, frontend/src/lib/RosterPanel.svelte, frontend/src/lib/RosterPanel.test.ts

### Acceptance Criteria
- An agent-layer toggle (`$state` boolean) exists in the canvas controls. When **off**, agent
  **nodes** are excluded from the hierarchy (e.g. `buildHierarchy($nodes.filter(n => n.type !==
  'agent'), …)` — `buildHierarchy` renders every root unconditionally, so the node gate is a
  pre-filter, distinct from the edge gate) **and** `authored_by` **edges** are filtered out (parity
  with today). When **on**, agent nodes render with a distinct `agent`-type style (via `node.type`
  threaded into `HierarchyNodeData`) and `authored_by` edges are drawn. Both the node gate and the
  edge gate are asserted via computed id sets (not pixels).
- `RosterPanel` renders one entry per roster `agentId`, colour-coded by `agentType`, showing an
  active vs inactive state; a roster update flipping a process to `inactive` updates the indicator;
  a respawn (new `processId`, same `agentId`) keeps a single agent entry shown as active. Per
  **Design Decision #1**, `RosterPanel` derives this per-`agentId` view from the P8-5 `agents` store
  (which is keyed by `processId`): an `agentId` is active iff **any** of its processes is active.
- Clicking a roster agent highlights the set of code nodes linked to `agent:<agentId>` by
  `authored_by` (asserted via the computed highlighted-id set, not pixels).
- Selecting a code node surfaces the agent ids that authored it (the agents-for-node mapping is
  exposed to the panel/sidebar and asserted in a component test).
- `buildHierarchy` now carries `node.type` into the render data and agent nodes receive the agent
  class (asserted in a `layout.ts` unit test).

### Definition of Done
- New vitest + `@testing-library/svelte` cases (`RosterPanel.test.ts`) and `layout.ts` unit tests
  cover toggle gating, roster rendering, active/inactive, both drill-down directions, and type→class
  threading; `npm test` green; new-code coverage ≥ 90%.
- `npm run check` zero errors; `npm run lint` clean.
- **UI validated in the running app** per CLAUDE.md: `just run`, load http://localhost:5173, drive a
  CLV activity line through `.lattice/clv.ndjson`, screenshot the agent toggle + roster panel + a
  drill-down via the Claude-in-Chrome MCP, and confirm no console errors (before/after captured).
- TSDoc/prop docs on `RosterPanel` and the new `layout.ts` helpers; the `frontend/README.md`
  event-flow section re-checked for the agent layer (`AGENT_PROTOCOL.md §6`).

---

## Dependency graph

```
P8-1  Wire payloads + storage no-op stubs + agent-node id (wire.rs, sqlite.rs, postgres.rs) . Depends: none
  ├─ P8-2  Activity → agent node + authored_by + roster (graph.rs, collector.rs) ........... Depends: P8-1
  │    └─ P8-4  Roster idle-timeout inactive + respawn (collector.rs, graph.rs) ........... Depends: P8-2
  ├─ P8-3  Persist agents/agent_activity/protocol_versions; replace P8-1 stubs (storage) ... Depends: P8-1
  └─ P8-5  Frontend ingest: roster state + reducer (types.ts, ws.ts) ...................... Depends: P8-1
       └─ P8-6  Agent-layer view: panel, styling, drill-down, toggle (UI) ... Depends: P8-5, P8-2, P8-4
```

- Acyclic. **Backend contract (P8-1) gates everything.** P8-1 adds the wire variants **and** the
  temporary no-op `persist` stubs in `sqlite.rs`/`postgres.rs` so `main` stays compilable + `just qg`
  green at its merge. After P8-1, backend behaviour (P8-2), storage (P8-3), and frontend ingest
  (P8-5) proceed in parallel — P8-1 is already merged, so P8-3's storage edits (which replace P8-1's
  stub arms) start from P8-1's merged state, not a live collision with P8-2 (graph/collector) or P8-5
  (types/ws).
- P8-2 → P8-4 share `graph.rs` + `collector.rs`; the edge serialises them (no parallel collision).
- P8-6 depends on **P8-5** (the frontend contract/store), **P8-2** (emits agent nodes/`authored_by`/
  roster), and **P8-4** (live `inactive` flip) — all three are required for its live-validation DoD to
  pass. P8-6 deliberately does **not** touch `ws.ts`/`types.ts` (P8-5 owns the contract); it only
  imports the `agents` store + `AgentInfo` and edits the canvas/panel files.
- Phase-8 accept (BUILD_PLAN) is met when P8-2 (touched nodes + authored_by + live status), P8-4
  (respawn shows new pid; active/inactive), and P8-6 (bidirectional drill-down view) are all merged;
  P8-3 makes the layer durable across a backend restart.
