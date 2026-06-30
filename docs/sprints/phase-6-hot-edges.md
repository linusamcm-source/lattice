# Lattice Phase 6 — Live call-path (hot edges)

Adds **hot edges**: a runtime call path lights up in real time as code executes and clears on exit.
A runtime `tracing` subscriber emits `#CLV1 {"event":"hotedge",…,"state":"enter|exit"}` lines; the
existing collector ingests them; the backend toggles `Edge.hot`; the frontend animates the live edge.
The high-frequency nature of call-path tracing is the phase's main risk, so **throttling/coalescing**
is a first-class deliverable — a hot loop must not flood the collector or the WebSocket
(BUILD_PLAN.md Phase 6; SPEC §6.3, §11.2). System-level **done** (BUILD_PLAN.md Phase 6 "Accept when"):
running code lights its call path in real time and clears on exit, without overwhelming the collector
under a hot loop.

**Grounding (read this session; Phases 0–5 merged on `main`).** Phase 5 already built the read side of
the `hotedge` wire word and the collector that will carry hot-edge envelopes — Phase 6 only has to act
on it:
- `crates/backend/src/clv.rs` — `ClvEvent::HotEdge { session, pid: Option<u32>, agent: Option<String>,
  msg: Option<String>, edge: String, state: String }` is **already parsed and tested** (clv.rs:87–100;
  `parses_hotedge_event` clv.rs:193; a `hotedge` line missing `edge` is rejected, clv.rs:233). `state`
  is a free `String` carrying `"enter"`/`"exit"`. No parser change is needed.
- `crates/backend/src/wire.rs` — `EventType::HotEdge` already exists and serialises to `"hot_edge"`
  (wire.rs:166–167). `Edge` already has a `pub hot: bool` field (wire.rs:279). The `Payload` enum
  (wire.rs:300–406) has **no** `HotEdge` variant — it holds only `Subtree`/`Snapshot`/`NodeUpsert`/
  `EdgeUpsert`/`NodeRemove`/`EdgeRemove`/`TestResult`/`StatusUpdate`. The enum is `#[serde(untagged)]`
  and **variant order is load-bearing** (wire.rs:286–297): a new variant must be placed so its required
  fields disambiguate it from the others under untagged decode.
- `crates/backend/src/graph.rs` — `Graph` owns `edges: HashMap<String, Edge>` keyed by `Edge::id`
  (graph.rs:56). `Graph::apply_clv` (graph.rs:257) folds a `ClvEvent` onto graph state and returns an
  optional patch envelope stamped by `Graph::envelope` (graph.rs:310). Today
  `ClvEvent::Activity { .. } | ClvEvent::HotEdge { .. } => None` is a **deliberate Phase-5 no-op**
  (graph.rs:305), asserted by `apply_clv_hotedge_is_a_noop_for_colour` (graph.rs:699–713). Phase 6
  **replaces** that no-op for `HotEdge`.
- `crates/backend/src/collector.rs` — `poll_once` parses each appended sink line with `parse_clv_line`,
  calls `graph.lock().await.apply_clv(&event)`, and broadcasts any returned envelope on `events_tx`
  (collector.rs:143–148). So **once `apply_clv` returns a `hot_edge` envelope, it flows to clients with
  zero collector change.** The sink is `<watched-repo>/.lattice/clv.ndjson` (`SINK_RELATIVE`,
  collector.rs:62).
- `frontend/src/lib/ws.ts` — `applyEvent` is the pure reducer over `GraphState { nodes, edges: Map }`
  (ws.ts:59–114); `KNOWN_EVENT_TYPES` (ws.ts:116) gates accepted discriminants and `isValidPayload`
  (ws.ts:136) shape-checks payloads; `parseEnvelope` rejects unknown `type`s (ws.ts:168). The `edges`
  derived store (ws.ts:184) already exposes each `Edge` (incl. `hot`).
- `frontend/src/lib/types.ts` — `EventEnvelope` is a discriminated union (types.ts:173–181) and
  `EventType = EventEnvelope['type']` (types.ts:184); `Edge` mirrors wire (`hot: boolean`). There is no
  `hot_edge` arm yet.
- `frontend/src/lib/layout.ts` — `buildEdges` (layout.ts:252) maps each CLV `Edge` to a SvelteFlow
  `FlowEdge` carrying `HierarchyEdgeData { kind, flowClass }` (the type is at layout.ts:198–209) and
  sets `animated: flowClass === 'data'` as the data-flow cue (layout.ts:270); `Graph.svelte` binds the
  result to `<SvelteFlow bind:edges>` (Graph.svelte:131,141). This is where a hot edge gets its
  **dedicated** hot styling, additive to (not replacing) the data-flow `animated` cue.

**Transport decision (deliberate, grounded — extends SPEC §6.3/§11.2; CLAUDE.md "extend the contract
and note it").** SPEC §11.2 offers two transports for hot edges: line-based stdout or a dedicated
binary channel. Phase 6 reuses the **existing Phase-5 `#CLV1` sink-file transport** (one
`#CLV1 {json}` line per enter/exit appended to `.lattice/clv.ndjson`) — no new binary channel. Rationale:
the collector, parser, correlation, and broadcast for `hotedge` lines already exist and are tested, so
the cheapest correct path is to emit `hotedge` lines onto the same sink. **Flooding is controlled at two
layers** (both required by §11.2 and both independently tested): (1) the **source** `tracing` subscriber
coalesces/samples rapid enter/exit so a hot loop emits a bounded number of lines (P6-3), and (2) the
**graph** emits a `hot_edge` envelope only on a genuine `hot` **transition** (P6-2), so even an unbounded
line stream produces at most one envelope per state change. A binary channel is explicitly **out of
scope** unless a P6-3 bench proves the line transport floods (it must not, given the two guards) — note
the result rather than building the channel speculatively.

**Scope discipline (BUILD_PLAN.md Phase 6).** In scope: `hot_edge` wire payload; `Graph::apply_clv`
hot-edge toggle with transition-coalescing; a reference runtime `tracing` subscriber emitter with
source throttling; frontend hot-edge animation. **Out of scope:** persistence of hot-edge records
(Phase 7 storage/`sqlx` — hot state stays in the in-memory `Graph` and is broadcast live only, never
written to a DB); the agent layer (Phase 8); WebSocket reconnect/resync of hot state (Phase 9 — hot is
transient and simply re-derives from new enter/exit lines). The collector, parser, node-id contract,
lazy pipeline, and all Phase 0–5 tests are unchanged. Backend stays **panic-free** on malformed input
(a bad `hotedge` line is skipped; an `enter`/`exit` for an edge id absent from the graph is ignored —
no error).

**Commands.** Backend `just qg` (= `fmt-check lint test`) / `just test`; frontend (from `frontend/`,
real npm `/opt/homebrew/bin/npm`) `npm run check` / `lint` / `test` / `build`. Coverage gate **90%**
new-code (`cargo llvm-cov`). Doc-comment cascade required (AGENT_PROTOCOL.md §6): every touched element
gets/updates its `///`/`//!` (or TSDoc) and the change cascades up to the module/`lib.rs` doc. Frontend
visual changes must be validated in the **running UI** (`just run` → Claude-in-Chrome / Playwright,
before/after screenshots), not from a passing unit test alone. Target branch `main`.

---

## Story P6-1: Wire contract — `hot_edge` payload (Rust + TS reducer in lockstep)

Lands the `hot_edge` event on the CLV seam end-to-end on the data layer, keeping `wire.rs`,
`types.ts`, and `DATA_MODEL.md` §A.5 in lockstep (CLAUDE.md "CLV is the seam — change all three
together"), exactly as P5-1 did for `test.result`/`status.update`. Adds `Payload::HotEdge` to the Rust
`Payload` enum (wire.rs:300; `EventType::HotEdge` already exists, wire.rs:166) with the §A.5 fields
`{ edgeId, state, processId, sessionId, agentId, ts }`, and a typed `HotEdgeState` (`enter|exit`)
mirroring the `NodeStatus`/`TestOutcome` enum idiom. Mirrors it in `types.ts` (a new `hot_edge` arm on
the `EventEnvelope` union + a `HotEdgePayload`) and folds it in `ws.ts`: `applyEvent` flips the matching
edge's `hot` flag immutably, `KNOWN_EVENT_TYPES` admits `hot_edge`, and `isValidPayload` shape-checks
it. This story does **not** produce or animate hot edges — it only defines and validates the contract
and the store fold.

### Depends On: none
### Touches: crates/backend/src/wire.rs, frontend/src/lib/types.ts, frontend/src/lib/ws.ts, frontend/src/lib/ws.test.ts

### Acceptance Criteria
- A `Payload::HotEdge { edge_id, state, process_id, session_id, agent_id, ts }` exists in `wire.rs`
  with serde renames matching `DATA_MODEL.md` §A.5 exactly (`edgeId`, `state`, `processId`, `sessionId`,
  `agentId`, `ts`); `state` is a typed `HotEdgeState` serialising to `"enter"`/`"exit"`; optional
  fields (`processId`, `agentId`) use `skip_serializing_if = "Option::is_none"` per the file's idiom.
- A Rust unit test round-trips a `hot_edge` `EventEnvelope` (serialise → JSON → deserialise) and asserts
  the JSON object has `type:"hot_edge"` and `payload.edgeId`/`payload.state` with the §A.5 spelling; the
  untagged `Payload` decode still resolves all existing variants unchanged (a `test.result`/`status.update`
  object does **not** mis-decode as `HotEdge`, and vice-versa).
- `types.ts` gains a `HotEdgePayload` and a `| (EnvelopeBase & { type: 'hot_edge'; payload: HotEdgePayload })`
  arm; `EventType` therefore includes `'hot_edge'`; `Edge.hot` typing is unchanged.
- `applyEvent(state, { type:'hot_edge', payload:{ edgeId:'e:a->b', state:'enter', … } })` returns a new
  `GraphState` whose `edges.get('e:a->b').hot === true`; `state:'exit'` sets it `false`; an `edgeId` not
  in `state.edges` returns the state unchanged (no phantom edge); the input state object is never mutated.
- `parseEnvelope` accepts a well-formed `hot_edge` message (returns the typed envelope) and rejects one
  whose `payload.edgeId` or `payload.state` is missing/non-string (returns `null`); `KNOWN_EVENT_TYPES`
  contains `'hot_edge'`.

### Definition of Done
- New Rust + Vitest tests written and green; new-code coverage ≥ 90%.
- `just qg` clean (fmt-check, clippy `-D warnings`, `cargo test --all`); `npm run check` + `lint` +
  `test` clean.
- Doc cascade: `Payload::HotEdge` and `HotEdgeState` carry `///` docs citing §A.5; the `wire.rs` /
  `types.ts` / `ws.ts` module docs are updated to list `hot_edge` alongside the other event types.
- `DATA_MODEL.md` is unchanged (§A.5 already specifies the payload); if any field spelling differs from
  §A.5 it is reconciled to the doc, not invented.

## Story P6-2: `Graph::apply_clv` — toggle `Edge.hot` with transition-coalescing

Makes the backend act on a `hotedge` event. Replaces the Phase-5 no-op arm
(`ClvEvent::HotEdge { .. } => None`, graph.rs:305) so an `enter` sets the target edge's `hot=true` and
an `exit` sets it `false`, returning a `hot_edge` `EventEnvelope` (built via `Graph::envelope`,
graph.rs:310, with `EventType::HotEdge` + `Payload::HotEdge` from P6-1) for the collector to broadcast.
**Coalescing is the load-bearing behaviour:** the envelope is emitted **only on a genuine state
transition**, so a hot loop re-entering an already-hot edge produces no further envelopes — this is the
backend half of the §11.2 anti-flood guard. The existing `apply_clv_hotedge_is_a_noop_for_colour` test
(graph.rs:699) is **replaced** by transition tests.

### Depends On: P6-1
### Touches: crates/backend/src/graph.rs, crates/backend/src/lib.rs

### Acceptance Criteria
- `apply_clv(&ClvEvent::HotEdge{ edge, state:"enter", .. })` on a graph that **contains** `edge` sets
  that edge's stored `hot` to `true` and returns `Some(EventEnvelope)` with `event_type == EventType::HotEdge`
  and a `Payload::HotEdge` whose `edge_id == edge`, `state == enter`, and `session_id`/`process_id`/`agent_id`
  echo the event's `session`/`pid`/`agent`.
- A second consecutive `enter` on the **same already-hot** edge returns `None` and leaves `hot == true`
  (transition-only emission — no duplicate envelope under a hot loop). An `exit` on a hot edge returns
  `Some(..)` with `state == exit` and sets `hot == false`; a second `exit` (already cold) returns `None`.
- `apply_clv` for a `hotedge` whose `edge` id is **not** in `self.edges` returns `None` and mutates
  nothing (mirrors the absent-node contract for `test`/`status`); a `state` string that is neither
  `"enter"` nor `"exit"` returns `None` and mutates nothing (panic-free on garbage).
- A `hotedge` event never changes any `Node::status` (it touches only `edges`); the existing
  `test`/`status`/`activity` arms of `apply_clv` are behaviourally unchanged (their tests still pass).

### Definition of Done
- The no-op hot-edge test is replaced by enter/exit/transition/absent-edge/bad-state tests, all green;
  new-code coverage ≥ 90%.
- `just qg` clean.
- Doc cascade: the `apply_clv` doc comment (graph.rs:234–256) is rewritten to describe the Phase-6
  hot-edge toggle + transition-coalescing (replacing the "hotedge is a no-op" sentence); the `graph.rs`
  and `lib.rs` module docs are re-checked so the Phase-5 "hotedge is a no-op" note no longer misleads.

## Story P6-3: Runtime `tracing` subscriber — reference hot-edge emitter with source throttle

Delivers the **producer** named in BUILD_PLAN.md Phase 6 ("runtime `tracing` subscriber emits hotedge
enter/exit") and de-risks the transport (BUILD_PLAN.md dependency note: "prototype the transport … and
measure under a hot loop before committing"). Adds a self-contained `tracing` `Layer` (new module
`crates/backend/src/tracing_layer.rs`, declared in `lib.rs`) that, on span **enter**/**close**, writes a
`#CLV1 {"event":"hotedge",…,"edge":"e:<from>-><to>","state":"enter|exit"}` line to a provided writer
(the sink file / stdout — the same Phase-5 `#CLV1` transport, no binary channel). It owns the **source
throttle**: rapid repeat enter/exit on the same edge within a coalescing window collapse to bounded
output, so a tight loop does not emit one line per iteration. The emitter is a reference/library
component (it does not have to be auto-wired into Lattice's own `app::run`); its value is a tested,
greppable demonstration that the line transport survives a hot loop.

### Depends On: none
### Touches: crates/backend/src/tracing_layer.rs, crates/backend/src/lib.rs

### Acceptance Criteria
- Given a span/edge-id mapping, entering a span writes exactly one well-formed `#CLV1` `hotedge`
  `enter` line and closing it writes one `exit` line, each parseable by `parse_clv_line` back into a
  `ClvEvent::HotEdge` with the matching `edge` and `state` (the emitter and the existing parser agree on
  the wire format).
- **Throughput bound (the §11.2 guard):** coalescing is **time-windowed** — within a fixed coalescing
  window of duration `W`, each edge emits at most a fixed constant number of lines no matter how many
  enter/exit cycles occur in that window. A test drives N ≥ 1000 enter/exit cycles on the **same** edge
  within a single window `W` and asserts the emitted-line count stays under a small fixed cap (**O(1) in
  N**, not O(N)). The sampler MUST be window-based for the per-cycle bound to hold: pure per-transition
  emission ("drop-while-hot") is **not** sufficient — it emits 2N lines for N enter/exit cycles. The
  window duration `W` and the per-window cap are the engineer's choice; the time-windowed,
  O(1)-emissions-per-window-per-edge bound is the contract. (Inject `W` and a clock/time-source so the
  test is deterministic — do not sleep on the wall clock.)
- Distinct edges are **not** coalesced together: interleaved enter/exit across two different edge ids
  each still produce their own transitions (throttling is per-edge, not global).
- The emitter writes to an injectable writer (e.g. `impl Write`) so the test captures output without a
  real file/stdout; it is panic-free and never blocks the traced program on a slow/failed write (a write
  error is dropped, not propagated).

### Definition of Done
- Unit tests (incl. the N-cycle throughput-bound test) written and green; new-code coverage ≥ 90%.
- `just qg` clean; `tracing_layer` is declared and documented in `lib.rs`.
- Doc cascade: the new module carries a `//!` doc explaining its role and the chosen throttle strategy
  with a one-line rationale; `lib.rs`'s module-level doc lists the new component and notes the
  line-transport-vs-binary-channel decision (transport kept line-based; binary channel not needed —
  cite the throughput-bound test as evidence).

## Story P6-4: Frontend — animate hot edges on the canvas, clear on exit

Closes the loop visually: an edge whose store `hot` flag is `true` renders **animated/highlighted** on
the SvelteFlow canvas and reverts when it goes cold. The `edges` store already carries `hot` (ws.ts:184)
and P6-1's reducer flips it on `hot_edge` envelopes, so this story is render-only: `buildEdges`
(layout.ts:252) sets a **dedicated** hot `class`/`style` from `edge.hot` — distinct from, and additive
to, the pre-existing `animated: flowClass === 'data'` data-flow cue (layout.ts:270) — while `Graph.svelte`
already binds `flowEdges` to `<SvelteFlow bind:edges>` (Graph.svelte:131,141), and `app.css` carries the
hot-edge styling. Must be **validated in the running UI**, not just unit tests (CLAUDE.md frontend rule).

### Depends On: P6-1
### Touches: frontend/src/lib/layout.ts, frontend/src/lib/Graph.svelte, frontend/src/lib/layout.test.ts, frontend/src/lib/Graph.test.ts, frontend/src/app.css

### Acceptance Criteria
- `buildEdges` maps a CLV `Edge` with `hot === true` to a SvelteFlow `FlowEdge` carrying a **dedicated**
  hot marker — a `hot` flag in `HierarchyEdgeData` plus a distinguishing hot `class`/`style` (e.g. a hot
  stroke colour + CSS pulse) — that is **independent of** the `animated` flag. The pre-existing
  `animated: flowClass === 'data'` data-flow cue (layout.ts:270) is preserved unchanged: a cold
  data-flow edge stays `animated === true` and a cold control-flow edge stays `animated === false`, and
  neither carries the hot marker; a `hot === true` edge carries the hot marker regardless of flow class.
  Asserted by a `layout.test.ts` unit test.
- Hot styling is **independent of** the existing control-flow/data-flow visibility toggles: a hidden
  edge class stays hidden even when `hot`, and a visible hot edge keeps its `kind`-based colour plus the
  hot animation overlay (no regression to P4-4 edge filtering).
- A `Graph.test.ts` test drives a `hot_edge` `enter` envelope through `applyEvent`/the store and asserts
  the derived flow edge gains the hot marker; a following `exit` removes it. The edge's `animated`
  data-flow cue (if any) is unchanged throughout.
- **Running-UI validation:** with `just run`, appending a `hotedge` `enter` line for a visible call edge
  to the watched repo's `.lattice/clv.ndjson` makes that edge animate within ~1s; an `exit` line clears
  it. Captured as before/after screenshots via Claude-in-Chrome/Playwright with a clean browser console
  (no errors).

### Definition of Done
- New Vitest unit/component tests green; new-code coverage ≥ 90%; `npm run check` + `lint` + `test` +
  `build` clean.
- Before/after screenshots of an animating-then-clearing hot edge attached to the story; console clean.
- Doc cascade: `buildEdges` TSDoc and the `layout.ts` / `Graph.svelte` module docs updated to describe
  hot-edge animation; `app.css` hot-edge rule commented.

---

## Dependency graph

```
P6-1 (wire+TS contract) ──► P6-2 (graph toggle, Rust)
        │
        └──────────────────► P6-4 (frontend animate, TS)

P6-3 (tracing emitter, Rust) ── independent
```

- **Wave 1 (parallel):** P6-1, P6-3 — no `Depends On`, disjoint `Touches`.
- **Wave 2 (after P6-1):** P6-2 (Rust, `graph.rs` + `lib.rs`), P6-4 (TS, `layout.ts`/`Graph.svelte`).
- Acyclic. P6-2 and P6-3 **both** declare `crates/backend/src/lib.rs` in `Touches` — P6-2 rewrites the
  stale "hotedge is a no-op" module-doc note there, P6-3 adds the `tracing_layer` module declaration —
  so the graph builder **serialises P6-2 and P6-3** on that shared `Touches` path even though P6-3 has
  no `Depends On`. Every other story pair touches disjoint files and is ordered purely by `Depends On`.
- End-to-end accept ("running code lights its call path and clears on exit, under a hot loop") is
  exercised by P6-4's running-UI criterion (toggle + animate) over P6-1/P6-2 (contract + transition
  guard), with P6-3 proving the producer + source throttle in isolation.
