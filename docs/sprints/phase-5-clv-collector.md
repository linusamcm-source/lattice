# Lattice Phase 5 — Test Tracking (CLV collector)

Adds the **CLV collector**: Lattice ingests `#CLV1`-tagged event lines, correlates them by
`session`/`pid`/`agent`, and turns `test`/`status` events into live node colouring (a failing test
reddens its node within ~1s). Today the backend produces structure only (Phases 0–4: nodes, docs,
signatures, edges) and every node's `status` is `Unknown` except a parse `Error`. System-level "done"
(BUILD_PLAN.md Phase 5): a failing test reddens its node within ~1s; a concurrent run in another
session never contaminates it (distinct `session`/`pid`); untagged/malformed lines are ignored.

**Transport decision (deliberate, grounded — note vs SPEC §8).** SPEC §8 says the backend "reads every
process's stdout in parallel (a `tokio` task per process)". Phase 5 instead **tails a sink file** at
`<watched-repo>/.lattice/clv.ndjson` — one `#CLV1 {json}` line per event, appended by an external
emitter (e.g. a Claude Code `PostToolUse` hook or a test reporter). Rationale: a Claude-session/agent's
stdout never reaches an external reader, so a sink file is the realistic transport. This was **verified
end-to-end this session**: a real hook in the surf-seer repo appended
`#CLV1 {"event":"activity","agent":"claude","session":"dfd7ce20-9761-4e2e-aeac-3b8931cf5d30","pid":17969,"node":"file:src/config/index.ts","action":"modified"}`,
which parsed valid. The spec's **non-contamination** requirement is satisfied per-event by the
`session`/`pid` fields each line carries — not by per-process readers. This extends SPEC §8 deliberately
(CLAUDE.md: extend the contract and note it); `DATA_MODEL.md` §A.4/§A.5 event/payload shapes are
unchanged.

**Grounding (read this session; Phases 0–4 merged on `main`):**
- `crates/backend/src/wire.rs` — `EventType` already has `StatusUpdate` (`"status.update"`),
  `TestResult` (`"test.result"`), `AgentActivity` (`"agent.activity"`), `HotEdge` (`"hot_edge"`)
  discriminators (wire.rs:131–142). The `Payload` enum (wire.rs:273–319) has **only**
  `Subtree`/`Snapshot`/`NodeUpsert`/`EdgeUpsert`/`NodeRemove`/`EdgeRemove` — **no** `StatusUpdate`/
  `TestResult` variants. `NodeStatus` already has `Unknown|Passing|Failing|Running|Stale|Error`
  (wire.rs:78–90). `EventEnvelope` carries `v|ts|sessionId|type|payload` (wire.rs:326+).
- `crates/backend/src/graph.rs` — `Graph` (`Arc<Mutex<Graph>>`) is the source of truth; `apply_parsed`
  diffs a file into patch envelopes. Nodes are keyed by deterministic id (`node_id`). No method yet to
  set a node's `status` from an external event.
- `crates/backend/src/app.rs` — `run(root, addr)` canonicalises `root`, builds the `Graph` + a
  `broadcast::Sender<EventEnvelope>` (`events_tx`, capacity 1024, app.rs:119), initial-parses via
  `WalkDir`, `serve`s, and spawns the **watcher pump** (app.rs:138–144): `watch(root,…) → mpsc →
  ingest_file → events_tx.send(event)`. `RunHandle` (app.rs:37–52) holds `watcher_task`/`pump_task`
  `JoinHandle`s and aborts them in `shutdown`. The collector is a new task wired in **exactly here**.
  Node ids are repo-relative (`repo_relative`, app.rs:60) — so `file:src/config/index.ts` from the
  emitter matches a node Lattice built for that watched repo.
- `crates/backend/src/watcher.rs` — the `tokio`/`notify` debounced watcher is the structural model for
  the collector's tailer task (a `tokio::spawn` emitting parsed events; `DEBOUNCE` constant).
- `frontend/src/lib/ws.ts` — `parseEnvelope` validates every message and **rejects unknown `type`s**;
  `applyEvent` is the pure reducer folding an envelope into `GraphState` (Maps by id). It handles
  `snapshot|subtree|node.upsert|node.remove|edge.upsert|edge.remove` — **not** `status.update`/
  `test.result`. `frontend/src/lib/types.ts` — `EventEnvelope` discriminated union + `NodeStatus`
  (mirrors wire). `frontend/src/lib/HierarchyNode.svelte` + `layout.ts` (`buildHierarchy`) render the
  node label/expander but **do not** read `node.status` yet (all nodes are `unknown`).

**Scope discipline (BUILD_PLAN.md Phase 5):** CLV collector + `#CLV1` parse + session/pid/agent
correlation + `status.update`/`test.result` → node colouring + untagged/malformed passthrough(ignore).
**No storage/sqlx** (Phase 7 — the collector is in-memory, emits live only, persists nothing). **No hot
edges** (Phase 6 — a `hotedge` line is parsed into the `ClvEvent` vocabulary but not acted on). **No
agent-layer UI** (Phase 8 — `activity` lines are parsed + correlated but do not render an agent node;
see the activity-scope note). Collector/parser are **panic-free** on malformed input (a bad line is
skipped, the tail continues). Node ids/structure, the lazy pipeline, and all Phase 0–4 tests are
unchanged.

**Activity-event scope (decided, not ambiguous):** `NodeStatus` has no "touched" state, and `activity`
attribution is the agent layer (Phase 8). So in Phase 5 an `activity` event is **parsed and
correlated but does not change a node's colour** (it is a no-op for status). The colouring deliverable
is driven by `test` (→ `Passing`/`Failing` via outcome) and `status` (→ `Running`/etc) events only.

**Node-id contract (emitter ↔ graph):** the emitter MUST use ids relative to the watched repo root
(`file:<relpath>`, `fn:<relpath>:<symbol>`) so they equal Lattice's `node_id` for that repo. A CLV
event whose node id is not in the graph is ignored (no colour, no error) — stated in P5-3/P5-4 ACs.

**Commands:** backend `just qg`/`just test`; frontend (from `frontend/`, real npm
`/opt/homebrew/bin/npm`) `npm run check`/`lint`/`test`/`build`. Coverage gate **90%** new-code.
Target branch `main`. Real proving ground: surf-seer's `.lattice/clv.ndjson` sink (one real line verified).

---

## Story P5-1: Wire contract — status.update + test.result payloads (Rust + TS reducer)

Add the two event payloads to both sides of the CLV seam. **Rust** (`crates/backend/src/wire.rs`): add
`Payload::TestResult` and `Payload::StatusUpdate` struct variants matching `DATA_MODEL.md` §A.5 —
`TestResult { node_id (serde "nodeId"), test_id (serde "testId"), outcome (a TestOutcome enum
pass|fail|skip|running), duration_ms?, session_id (serde "sessionId"), agent_id?, process_id?,
message? }`; `StatusUpdate { node_id (serde "nodeId"), status (a NodeStatus or status enum, serde
"status"), session_id (serde "sessionId"), agent_id?, process_id? }`. Wire them to the existing
`EventType::TestResult`/`StatusUpdate` discriminators; round-trip through the `EventEnvelope`.
**`Payload` is `#[serde(untagged)]` and variant order is LOAD-BEARING for decode (wire.rs:263) — this
is the load-bearing constraint of this story.** Declare the two new variants **after** the existing six
and order **`TestResult` before `StatusUpdate`**, because an untagged struct variant ignores unknown
fields: a `test.result` object `{nodeId,testId,outcome,sessionId,…}` carries all of `StatusUpdate`'s
required fields, so if `StatusUpdate` came first it would mis-decode the test result as a status update
and silently drop `testId`/`outcome`/`durationMs`. `TestResult` must require a field `StatusUpdate`
lacks — **`testId` is that disambiguator** (require `nodeId`,`testId`,`outcome`,`sessionId` on
`TestResult`; `nodeId`,`status`,`sessionId` on `StatusUpdate`). A `status.update` object (no `testId`)
fails `TestResult` and falls through to `StatusUpdate`. Mirror this ordering note in the doc-comment
exactly as the existing `Subtree`-before-`Snapshot` note does. **TS** (`frontend/src/lib/types.ts`): add the mirrored payload interfaces + the two
`EventEnvelope` union members; **`ws.ts`**: `parseEnvelope` accepts `status.update`/`test.result`
(no longer rejected as unknown), and `applyEvent` folds each onto the target node's `status` in
`GraphState` (immutably; a `test.result` outcome `fail`→`failing`, `pass`→`passing`; a `status.update`
sets the given status; a node id absent from the store is a no-op). No collector yet; no colour yet.

### Depends On: none
### Touches: crates/backend/src/wire.rs, frontend/src/lib/types.ts, frontend/src/lib/ws.ts, frontend/src/lib/ws.test.ts

### Acceptance Criteria
- A `test.result` `EventEnvelope` with `payload.nodeId == "fn:a.rs:f"`, `outcome == "fail"` serialises to
  JSON with `"type":"test.result"` and a `"nodeId"` key, and round-trips back equal (Rust).
- A `status.update` envelope round-trips with `"type":"status.update"` and its `nodeId`/status preserved (Rust).
- **Untagged disambiguation (the load-bearing test):** deserializing a `test.result` payload object
  (`{nodeId,testId,outcome,sessionId}`) yields `Payload::TestResult` (with `testId` intact, not dropped),
  and a `status.update` payload object (`{nodeId,status,sessionId}`) yields `Payload::StatusUpdate` —
  neither mis-decodes as the other. A bare `{ "id": … }` still decodes to `Payload::NodeRemove`
  (existing `node.remove`/`edge.remove` behaviour unchanged).
- Existing Phase 0–4 wire round-trip tests still pass (no change to existing variants/ids).
- `parseEnvelope` returns a typed `test.result` envelope (not `null`) for a valid `test.result` message,
  and still returns `null` for a genuinely unknown `type` (TS).
- `applyEvent(state, <test.result fail for an existing node>)` returns a new state whose node has
  `status === "failing"`; for `pass`, `"passing"`; a `test.result`/`status.update` for a node id **not**
  in the store returns state with that node still absent (no crash, no phantom node).

### Definition of Done
- `rust-tester` RED tests first then `rust-developer`; `typescript-tester` then `typescript-developer`
  for the TS side; new code ≥90% line-covered.
- `just qg` green; `npm run check`/`lint`/`test`/`build` green.
- `///`/TSDoc on the new payloads; note the §A.5 mapping; keep wire.rs/types.ts/DATA_MODEL in lockstep
  (CLAUDE.md CLV-seam rule).

## Story P5-2: CLV line parser (#CLV1 → ClvEvent)

Add a pure, panic-free parser in a new module `crates/backend/src/clv.rs`: `parse_clv_line(line: &str)
-> Option<ClvEvent>`. `ClvEvent` is an enum over the four protocol event types (`Activity`, `Test`,
`Status`, `HotEdge`) carrying their `AGENT_PROTOCOL.md` §2.3 fields (`session`, `pid?`, `agent?`,
`node`/`edge`, `action`/`outcome`/`state`, `durationMs?`, `msg?`). The parser: returns `None` for any
line not starting with the exact `#CLV1 ` prefix (trailing space), for non-JSON after the prefix, for
JSON missing required fields (`event`, `session`; `node` for activity/test/status; `edge` for hotedge),
and for an unknown `event` value — **never panics, never errors** (ignore-and-continue is the contract).
Registered via `pub mod clv;` in `lib.rs`.

### Depends On: none
### Touches: crates/backend/src/clv.rs, crates/backend/src/lib.rs

### Acceptance Criteria
- `parse_clv_line("#CLV1 {\"event\":\"test\",\"session\":\"s1\",\"pid\":42,\"node\":\"fn:a.rs:f\",\"outcome\":\"fail\",\"durationMs\":14}")`
  returns `Some(ClvEvent::Test { .. })` with `node == "fn:a.rs:f"`, `outcome == fail`, `session == "s1"`.
- The real verified activity line
  `#CLV1 {"event":"activity","agent":"claude","session":"dfd7ce20-9761-4e2e-aeac-3b8931cf5d30","pid":17969,"node":"file:src/config/index.ts","action":"modified"}`
  parses to `Some(ClvEvent::Activity { node: "file:src/config/index.ts", action: modified, .. })`.
- A line without the `#CLV1 ` prefix (e.g. plain `cargo test` output) returns `None`.
- A `#CLV1 ` line with malformed JSON (`#CLV1 {not json`) returns `None` and does not panic.
- A `#CLV1 ` line with a valid JSON object but an unknown `event` (`"event":"bogus"`) or missing
  `session`/`node` returns `None`.
- An `#CLV1 {"event":"status","session":"s1","node":"fn:a.rs:f","outcome":"running"}` parses to
  `Some(ClvEvent::Status { outcome: running, .. })`.

### Definition of Done
- `rust-tester` RED table-driven tests first (valid + every reject path), then `rust-developer`; ≥90% new-code coverage incl. each reject branch.
- `just qg` green.
- `//!` module doc + `///` on `parse_clv_line`/`ClvEvent` stating the ignore-malformed contract; cascade `lib.rs` module list.

## Story P5-3: Graph status application from a CLV event

Add to `Graph` a method `apply_clv(&mut self, event: &ClvEvent) -> Option<EventEnvelope>` that maps a
correlated CLV event onto node colour: a `Test` event whose `node` id exists in the graph sets that
node's `status` (`fail→Failing`, `pass→Passing`, `skip→Stale` or unchanged, `running→Running`) and
returns a `test.result` `EventEnvelope` (stamped with the graph's session helper like `apply_parsed`);
a `Status` event sets the node's `status` and returns a `status.update` envelope; an `Activity` or
`HotEdge` event returns `None` (activity is no-op for colour per the scope note; hotedge is Phase 6).
A `Test`/`Status` event whose `node` id is **absent** from the graph returns `None` and mutates nothing.
The node's stored `Node.status` is updated so a later `snapshot`/`subtree` reflects the colour.

### Depends On: P5-1, P5-2
### Touches: crates/backend/src/graph.rs

### Acceptance Criteria
- With a graph containing `fn:a.rs:f` (status `Unknown`), `apply_clv(&Test{node:"fn:a.rs:f",outcome:fail,session:"s1",..})`
  returns `Some(env)` where `env.event_type == TestResult` and `env.payload` carries `nodeId == "fn:a.rs:f"`,
  and the stored node's `status` is now `Failing`.
- A subsequent `apply_clv(&Test{node:"fn:a.rs:f",outcome:pass,..})` flips the stored status to `Passing`
  and returns a `test.result` envelope.
- `apply_clv(&Status{node:"fn:a.rs:f",outcome:running,..})` sets status `Running` and returns a `status.update` envelope.
- `apply_clv(&Test{node:"fn:does.not.exist",..})` returns `None` and leaves the graph unchanged (node count and statuses identical).
- `apply_clv(&Activity{..})` returns `None` and changes no node status (activity is no-op for colour).
- Existing `apply_parsed`/snapshot/subtree tests still pass; a node coloured by `apply_clv` then re-`apply_parsed`
  (same source) keeps structure (status handling does not corrupt the diff).

### Definition of Done
- `rust-tester` RED tests first, then `rust-developer`; ≥90% new-code coverage incl. the absent-node and activity/hotedge no-op branches.
- `just qg` green.
- `///` on `apply_clv` stating the event→status mapping and the absent-node/activity contract; cascade `graph`/`lib.rs` docs.

## Story P5-4: Collector task — tail the sink, correlate, broadcast

Add a collector module (`crates/backend/src/collector.rs`) with a `tokio` task that **tails**
`<root>/.lattice/clv.ndjson`: it tolerates the file being absent at startup then created, reads only
**newly appended** complete lines (buffering a trailing partial line until its newline arrives), parses
each via `parse_clv_line` (P5-2), and for each correlated `Test`/`Status` event locks the shared
`Graph`, calls `apply_clv` (P5-3), and `events_tx.send`s the returned envelope so connected clients
recolour. Wire it into `app::run` like the watcher pump — spawn it after `serve`, add a
`collector_task: JoinHandle<()>` to `RunHandle`, abort it in `shutdown`. Follow semantics: poll the
file for growth on a short interval (≤500 ms, so a failing test reddens within ~1s) **or** use `notify`;
on truncation/rotation (new size < last offset) reset to the file start. Per-event `session`/`pid` keep
concurrent sessions from cross-colouring — the collector does not maintain per-session node state, it
applies each event to its own `node` id, so interleaved sessions writing the same sink never contaminate
each other (only same-node events from different sessions race, last-write-wins, which is correct).

### Depends On: P5-2, P5-3
### Touches: crates/backend/src/collector.rs, crates/backend/src/app.rs, crates/backend/src/lib.rs

### Acceptance Criteria
- Integration (mirroring app.rs tests): `run` over a tempdir containing `a.rs` (`fn f(){}`); after a
  client connects and expands `file:a.rs`, appending
  `#CLV1 {"event":"test","session":"s1","pid":1,"node":"fn:a.rs:f","outcome":"fail"}` to
  `<root>/.lattice/clv.ndjson` causes a `test.result` (or `status`-bearing) envelope for `fn:a.rs:f` to
  arrive over the WebSocket within ~1s, and a fresh `snapshot`/`subtree` shows that node `Failing`.
- The sink file **not existing** at startup does not crash `run`; creating it later and appending a line
  still delivers the event.
- A line appended in two writes (partial line, then the rest ending in `\n`) is parsed exactly once, only after the newline.
- An appended untagged line (`PASS app/foo.test.ts`) and a malformed `#CLV1 {` line produce **no**
  envelope and do not stop the tailer (a subsequent valid line is still delivered).
- Two interleaved sessions — `…"session":"s1"…"node":"fn:a.rs:f"…fail` then
  `…"session":"s2"…"node":"fn:a.rs:g"…pass` — colour `f` Failing and `g` Passing independently (no cross-contamination).
- `RunHandle::shutdown` aborts the collector task (no leaked task; existing shutdown test style).

### Definition of Done
- `rust-tester` RED async integration tests first (`tokio::test`), then `rust-developer`; ≥90% new-code coverage incl. absent-file, partial-line, malformed, and shutdown paths.
- `just qg` green.
- `//!`/`///` docs on the collector + its follow/poll semantics and the sink-path-from-root contract; cascade `app`/`lib.rs` module docs.

## Story P5-5: Frontend — colour nodes by status

Surface `node.status` as colour on the SvelteFlow canvas (today every node renders identically). Thread
`status` through `buildHierarchy` into `HierarchyNodeData` (`layout.ts`), and in `HierarchyNode.svelte`
apply a status→style mapping per SPEC §9.6: `passing`→green, `failing`→red, `running`→pulsing,
`stale`→grey, `error`→hatched, `unknown`→default. Because `applyEvent` already folds `test.result`/
`status.update` onto the node's `status` (P5-1), a delivered failing event recolours the node live with
no extra wiring. Keep it `any`-free and strict-clean; lazy discipline unchanged.

### Depends On: P5-1
### Touches: frontend/src/lib/layout.ts, frontend/src/lib/HierarchyNode.svelte, frontend/src/lib/layout.test.ts, frontend/src/lib/Graph.test.ts, frontend/src/app.css

### Acceptance Criteria
- A `@testing-library/svelte` test mounting `HierarchyNode` (in a `SvelteFlowProvider`) with
  `data.status === "failing"` renders an element carrying a failing/red class or style marker; with
  `"passing"` a passing/green marker; with `"unknown"` neither.
- `buildHierarchy` copies each CLV node's `status` into its node `data` (unit-tested for a `failing` node).
- In `Graph.svelte`/store flow: applying a `test.result` envelope (via `applyEvent`) whose `nodeId` is a
  currently-rendered node updates that node's rendered status marker to failing (proves the live recolour path).
- `npm run check`/`lint`/`test`/`build` green; no `any`.

### Definition of Done
- `typescript-tester` RED component/unit tests first, then `typescript-developer`; new colour logic ≥90% line-covered.
- `npm run check`/`lint`/`test`/`build` green.
- TSDoc on the status→style mapping; `frontend/README.md` notes status colouring.
