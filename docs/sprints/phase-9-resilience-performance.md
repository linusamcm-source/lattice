# Lattice Phase 9 — Resilience + performance

Phase 9 hardens the live pipeline so it **degrades gracefully and stays bounded**: the client
auto-reconnects and resyncs after a dropped socket, the backend rebuilds its in-memory graph from
the persisted DB on restart, every unbounded buffer/map grows under a cap, and a self-observability
metrics panel exposes how Lattice copes (parse latency, node/edge counts, memory, events/sec). This
is the `BUILD_PLAN.md` §"Phase 9" deliverable; it sits on top of Phase 7 (storage) and Phase 8
(agent layer), and folds in the three review findings deferred from Phase 8
(`.team-sprint/sprints/sprint-phase-8-agent-layer/deferred-phase-9.md`).

**System-level "done" (BUILD_PLAN.md §Phase 9 accept):** killing the backend and feeding malformed
code both degrade gracefully; the client resyncs cleanly after a forced disconnect. Cross-cutting
(BUILD_PLAN.md): "Reconnecting after a forced disconnect yields a graph identical to the server's
state." Behaviour/rationale: `SPEC.md` §11.1 (resilience), §11.2 (performance/throttling), §11.3
(self-observability). Contracts: `DATA_MODEL.md` §A (the CLV seam — `wire.rs` ⇄ `types.ts` ⇄
`DATA_MODEL.md` change in lockstep). Doc-comment cascade: `AGENT_PROTOCOL.md` §6.

## Grounding — what already exists (verified by recon this session)

Citations are `file:line` from this session's graphify + Read/Grep recon. Triple-verified negatives
are marked.

**Resilience — WS + crash-rebuild**

- **WS resync is already implemented server-side.** `ws.rs:135-138` sends a root-only `snapshot` on
  connect (subscribing to the broadcast channel *before* sending, `ws.rs:118-120,133`, so no event is
  missed). A client `{"type":"snapshot"}` frame re-sends a fresh snapshot (`is_snapshot_request`,
  `ws.rs:155-159,181-190`); `{"type":"expand","nodeId":…}` returns that node's direct children
  (`expand_request_node_id`, `ws.rs:160-165,200-209`). Both matchers are panic-free. So the **backend
  WS contract needs no change for basic resync** — `DATA_MODEL.md:71` defers re-expansion of open
  subtrees to the *client*.
- **The frontend has NO reconnect, backoff, or resync-send.** `connect(url)` (`ws.ts:304-312`)
  registers only a `'message'` listener — no `'close'`/`'error'`/`'open'` handler; the socket dies
  silently. Triple-verified: `grep -rniE "reconnect|backoff|retry|onclose|disconnected|connecting"`
  over `frontend/src/` returns only two *comments* (`ws.ts:21`, `ws.ts:299`) saying it lands in
  Phase 9. No function ever sends `{type:"snapshot"}`. No connection-status store/flag/component
  exists (`grep "Reconnecting"`, `isConnected|connectionState|ConnectionStatus` → 0 hits; the only
  panel is `RosterPanel.svelte`). `applyEvent` is a pure reducer: `snapshot` replaces the Maps
  wholesale (`ws.ts:80-84`), `subtree` merges by id (`ws.ts:105-114`).
- **Crash-rebuild-from-DB is entirely MISSING.** `Graph::new()`/`with_session` create an *empty*
  graph (`graph.rs:115,120-130`); `run_with_db_url` boots an always-empty `Arc<Mutex<Graph>>`
  (`app.rs:195`) and fills it **from the filesystem** by re-parsing every file (`app.rs:247-252`),
  never from the DB. No `from_storage`/`rehydrate`/`load` constructor exists (triple-verified: exact
  grep, case-insensitive grep, filename glob → 0 production hits; the only `.fetch_all` is a *test*
  at `sqlite.rs:918`).
- **The `Storage` trait is write-only.** `trait Storage` (`storage/mod.rs:135-154`) is exactly
  `ensure_schema`, `persist(env)`, `record_session` — no read method. Every `SELECT`/`fetch_*` in
  both backends sits inside `#[cfg(test)]` (sqlite tests start `sqlite.rs:586`, postgres
  `postgres.rs:677`). **But the data to rebuild already persists**: the `nodes`
  (`sqlite.rs:94-107`) and `edges` (`sqlite.rs:108-115`) tables are written by the `NodeUpsert`/
  `EdgeUpsert`/`node.remove`/`edge.remove` persist arms.
- **`Graph` fields are private** (`graph.rs:77-105`: `nodes`, `edges`, `file_nodes`, `file_edges`,
  `roster`, `last_seen`) → any rehydration constructor must live **in `graph.rs`**. The
  `file_nodes`/`file_edges` diff-tracking maps and the `roster`/`last_seen` maps have **no table** —
  not reconstructable from `nodes`/`edges` alone (so a rehydrate restores nodes/edges and must
  **reconcile via re-parse** for diff state; roster is repopulated by live activity).
- **Persistence is opt-in (`LATTICE_DB_URL`) and lossy** — the persist subscriber can lag and drop
  events (`app.rs:226-231`). The `store` handle is **moved into** the persist task (`app.rs:218`,
  `Box<dyn Storage>` not `Arc`) → reading at startup must happen **before** the spawn, or the store
  must become shareable.

**Performance — debounce, hot-edge, bounded memory**

- **Watcher debounce exists.** `DEBOUNCE = 150ms` (`watcher.rs:27`); `debounce_loop`
  (`watcher.rs:51-73`) coalesces a burst into a `HashSet<PathBuf>` and flushes when quiet.
  `is_source_file` accepts only `rs|py|ts` (`watcher.rs:35-40`). The single re-parse downstream
  (`app.rs:107-120` → `parse_source` → `apply_parsed`) produces AST + docs + dataflow **together**,
  so debounce covers all three. Tests: same-path coalescing (`watcher.rs:137,178,216`). **Gap:** the
  window *extends* on every event (`watcher.rs:57-60`) → a file written continuously every <150ms is
  **starved** (unbounded flush latency); `raw_tx` is unbounded (`watcher.rs:83`); no distinct-paths
  burst test.
- **Hot-edge throttle exists and is regression-locked.** `HotEdgeThrottle` (`tracing_layer.rs`):
  per-edge, per-state, fixed-window cap (`THROTTLE_WINDOW_MS=100`, `ENTER_CAP=2`, `EXIT_CAP=1`;
  pure `note(edge,state,now)` with injected clock, `tracing_layer.rs:156-186`). Test
  `throttle_bounds_emissions_per_window` fires **5,000 transitions** → asserts `emitted ≤ 3`
  (`tracing_layer.rs:351-376`). A second semantic-dedup layer in `Graph::apply_clv` drops re-enter
  of an already-hot edge (`graph.rs:318-321,452`). **Gap:** `HotEdgeThrottle.tallies` is **never
  evicted** (`tracing_layer.rs:117-121`) — "finite edge set" is an unproven assumption → unbounded
  map on a long run. Single `Mutex` over all edges (`tracing_layer.rs:257`).
- **Frontend bounded-memory (collapse-discard) is real and tested.** `collapse(state,nodeId)`
  (`ws.ts:343-364`) BFS-removes transitive descendants by `parentId` and drops edges touching them;
  `toggle` (`Graph.svelte:146-158`) clears the id from `expanded` *and* calls `collapse`. Tests
  assert two-level descendant removal + edge drop + purity (`ws.test.ts:302-343`). **Gaps:** discard
  happens **only** on explicit collapse (no zoom/LRU eviction); `collapse` follows `parentId` not
  `contains` edges (orphan-leak risk if a node lacks `parentId`); calling `collapse` directly leaves
  a stale `expanded` entry.
- **No shared rate-limit/debounce utility** (triple-verified: 0 non-test hits, 0 matching
  filenames). Each primitive is bespoke and local.

**Parser recovery**

- **Panic-free, but coarse.** `parse_source` (`parser/mod.rs:185-208`) dispatches `rs→syn`,
  `py/ts→tree-sitter`, else bare `file` node. Rust/`syn` is **all-or-nothing**: a syntax error
  returns *only* the file node `NodeStatus::Error`, **no partial tree** (`mod.rs:104-113`).
  Python/TS (shared `extract`) recover siblings but mark only a **file-level** Error via
  `root.has_error()` (`treesitter.rs:335-339`). One malformed test per path exists
  (`mod.rs:1080,1433`; `treesitter.rs:621,724`). **Spec drift:** BUILD_PLAN/SPEC say "offending node
  marked `error`, siblings live" — neither path marks the *offending* node, and Rust discards all
  siblings. No sibling-recovery test (Py/TS), no fuzz/property panic test across the dispatch.

**Metrics envelope — the CLV seam**

- **`EventType` is a separate enum** (`wire.rs:165-203`, 12 tags) carried on `EventEnvelope`
  (`wire.rs:539-553`, camelCase struct); **`Payload` is `#[serde(untagged)]`** with 11 variants
  (`wire.rs:327-509`) — **declaration order is load-bearing** for decode (`wire.rs:305-326`). The
  untagged enum has **no container `rename_all`** → every camelCase field needs an **explicit
  `#[serde(rename=…)]`**. `Payload`/`EventEnvelope` derive `Eq`. Envelopes are stamped via
  `Graph::envelope`/`envelope_at` (`graph.rs:647,656-637`). `protocol_sentinel()` lives in
  `lib.rs:109-110` (`"#CLV1"`), unchanged by additive types.
- **`types.ts` mirrors `wire.rs` 1:1** (`types.ts:219-244`). A new wire `type` must touch **five**
  frontend seams, not three: the union (`types.ts`), plus `KNOWN_EVENT_TYPES` (`ws.ts:171-183`) and
  `isValidPayload` (`ws.ts:212-234`) — which **silently drop** an unknown type — plus `applyEvent`
  (`ws.ts:78`).
- **Broadcast fan-out** is a `tokio::sync::broadcast::Sender<EventEnvelope>` (`app.rs:196`, cap 1024)
  cloned into producer tasks (watcher pump `app.rs:263-269`, collector `app.rs:273`); the persist
  task subscribes (`app.rs:217-236`). A periodic metrics task is a natural 4th `tokio::spawn`.

## Design decisions (deliberate, grounded — declared so Phase 1 review sees them as intentional)

1. **Crash-rebuild = DB warm-start, then filesystem reconcile.** Because persistence is lossy
   (`app.rs:226-231`) and the diff-tracking/roster maps aren't persisted, rehydrating from the DB
   restores `nodes`/`edges` for an instant non-empty first snapshot, then the existing WalkDir
   re-parse (`app.rs:247-252`) runs and `apply_parsed` reconciles any drift. Rehydrate is **best-
   effort**: any storage/read error logs and degrades to today's empty-then-parse path (mirrors the
   existing `None` degradation). When `LATTICE_DB_URL` is unset there is nothing to rebuild — no-op.
2. **Storage gains read methods; rehydrate keys on the run session; the store becomes shareable.**
   P9-1 adds `load_nodes(session)`/`load_edges(session)` to the trait + both impls (**no
   `latest_session`** — see below). P9-2 reads **before** spawning the persist task (or switches the
   handle to `Arc<dyn Storage + Send + Sync>`) to resolve the `store`-moved-into-task ownership at
   `app.rs:218`, and rehydrates on the **`RUN_SESSION_ID = "sess-local"` constant** (`app.rs:55`),
   **not** a most-recent-session lookup. Nodes/edges always persist under the run session
   (`graph.rs:58` stamps `DEFAULT_SESSION_ID = "sess-local"`), while CLV events lazily create
   *later-started* `sessions` rows under their own payload session (`sqlite.rs:233,360,382`), so a
   "latest row" query would select the wrong session and silently load zero nodes (adversarial
   review CRITICAL/HIGH).
3. **`metrics.update` is all-integer to preserve the `Eq` derive.** A floating `f64` latency/throughput
   field would break `Eq` on `Payload`/`EventEnvelope` and cascade. Fields are `nodeCount:u64`,
   `edgeCount:u64`, `memoryBytes:u64`, `eventsPerSecMilli:u64` (events/sec ×1000), and
   `parseLatency: Vec<FileParseLatency{ filePath:String, durationUs:u64 }>`. `memoryBytes` is a
   **deterministic estimate** computed from the in-memory map sizes (not platform RSS), so it is
   unit-testable. The variant is declared **last** in `Payload`, gated on the unique `nodeCount`/
   `parseLatency` shape so untagged decode can't mis-resolve.
4. **Snapshot carries roster via an additional `agent.roster` envelope on connect/resync — not a
   `Snapshot` schema change.** Deferral #3: a client connecting/resyncing after agent activity sees
   agent *nodes* (roots in snapshot) but an empty `RosterPanel`. Rather than widen the fixed
   `Payload::Snapshot{nodes,edges}` contract, `handle_connection` sends the snapshot **then** a
   roster envelope built from `Graph.roster` (reusing `roster_envelope`, `graph.rs:637`) when the
   roster is non-empty; the same on a `{"type":"snapshot"}` resync. Additive, no `DATA_MODEL.md`
   schema change.
5. **Parser story is a NON-DESTRUCTIVE regression-lock — it does NOT weaken the "offending node
   marked `error`" contract.** That invariant lives in four spec sites (`SPEC.md:125,230`,
   `BUILD_PLAN.md:10,68`) **and** the project `CLAUDE.md` (harness-OVERRIDE level), so an
   implementation agent cannot ratify downgrading it (adversarial review HIGH). P9-10 therefore only
   (a) regression-locks the behaviour that is true today — *never panic across all paths; tree-sitter
   keeps valid siblings live* (already real, `treesitter.rs:370-438`, no early return; Rust/`syn` is
   genuinely all-or-nothing, `mod.rs:104-113`) — and (b) documents the `syn` all-or-nothing limitation
   in the **parser module docs only** (`AGENT_PROTOCOL.md §6` cascade), touching **neither the product
   spec nor CLAUDE.md**. Fully *meeting* the cross-cutting criterion (surfacing offending tree-sitter
   ERROR nodes) and/or formally amending the spec for the `syn` path is a deliberate follow-up for the
   user to direct — out of Phase 9's resilience scope, not silently actioned here.
6. **`graph.rs` is touched by four backend stories (P9-1, P9-4, P9-7, P9-8) on disjoint sections.**
   They declare the overlapping `crates/backend/src/graph.rs` glob in `Touches` so the scheduler
   **serializes** them (no parallel collision), per the Phase-8 precedent.
7. **`from_records` re-derives `child_ids` from the loaded `contains` edges (adversarial review
   CRITICAL).** `Node.child_ids` (`wire.rs:273`) is part of `Eq` and is populated by the parser from
   `contains` edges (`parser/mod.rs:217-231`) but is **never persisted** (no column; `NodeUpsert`
   binds 12 fields without it, `sqlite.rs:301-318`), so `load_nodes` returns `child_ids: []`.
   `from_records` must re-derive them (reusing the parser's `populate_child_ids` rule: a parent
   node's `child_ids` = the targets of its `contains` edges) so a post-rebuild re-parse of an
   unchanged file diffs to an empty `Vec`. Node→file grouping for `file_nodes` follows the
   `parent_id` chain to the root `file:` node; edge→file grouping attributes each edge to its
   **source** node's file (covers cross-file `calls`/`data_flows_from`/`authored_by`).

---

## Story P9-1: Storage read methods + `Graph` rehydration constructor (backend)

Add the read side that crash-rebuild needs. Extend `trait Storage` (`storage/mod.rs:135-154`,
today write-only) with `load_nodes(session_id) -> Vec<Node>` and `load_edges(session_id) -> Vec<Edge>`,
implemented in both `sqlite.rs` and `postgres.rs` (the `nodes`/`edges` columns exist,
`sqlite.rs:94-115`). **No `latest_session`** (Design Decision #2 — P9-2 keys on the `RUN_SESSION_ID`
constant). Add a `Graph::from_records(session_id, nodes, edges) -> Graph` constructor in `graph.rs`
that bulk-loads the node/edge maps, **re-derives each node's `child_ids` from the loaded `contains`
edges** (Design Decision #7 — `child_ids` is unpersisted), and rebuilds the `file_nodes`/`file_edges`
diff-tracking maps using the grouping rule in Design Decision #7 (node→file by `parent_id` chain to
the root `file:` node; edge→file by source node's file). `roster`/`last_seen` start empty (repopulated
by live activity, per Design Decision #1). No `app.rs` wiring yet — pure read+construct surface.

### Depends On: none
### Touches: crates/backend/src/storage/mod.rs, crates/backend/src/storage/sqlite.rs, crates/backend/src/storage/postgres.rs, crates/backend/src/graph.rs

### Acceptance Criteria
- `trait Storage` exposes `load_nodes` and `load_edges` (async, returning `Result<…, StorageError>`);
  the trait stays object-safe (`Box<dyn Storage + Send + Sync>` still compiles).
- Against a `temp_store()` (`sqlite.rs:596`, file-backed) seeded by persisting the
  `node.upsert`/`edge.upsert` envelopes produced by parsing a real multi-file source set,
  `load_nodes(session)` returns every persisted node with `id`/`type`/`label`/`parentId`/`status`
  intact (note: `child_ids` is **not** persisted and loads as `[]`, re-derived by `from_records`,
  not by `load_nodes`), and `load_edges(session)` returns every edge with `source`/`target`/`kind`;
  a `node.remove`-then-`load` does not return the removed node.
- **`from_records` round-trips parse state losslessly.** Given the nodes/edges produced by
  `parse_source` of one or more files, `Graph::from_records("sess-local", loaded_nodes, loaded_edges)`
  yields a graph whose `snapshot()` is **order-independently equal** to the snapshot of a graph that
  **parsed the same files** — compare nodes/edges as **sets keyed by id** and `child_ids` as a
  **sorted/normalised** set, because `snapshot()` collects from unsorted `HashMap::values()`
  (`graph.rs:156-169`) so a raw `assert_eq!` on two distinct graphs is order-flaky (adversarial review
  MEDIUM). The **load-bearing** correctness proof is the next clause: a subsequent `apply_parsed` of
  any of those files **unchanged** returns an **empty** `Vec` (no spurious `node.upsert`/`node.remove`)
  — proving `child_ids` re-derivation and the `file_nodes`/`file_edges` rebuild are correct.
- The multi-file case is covered: a graph rebuilt from two files' records, re-parsing **one**
  unchanged file, emits an empty diff and does **not** remove the other file's nodes (edge→file
  grouping attributes cross-file edges to their source file).
- SQLite and Postgres `load_*` impls are structural twins; the Postgres impl is covered by the
  existing Docker-gated parity harness (skipped when no `LATTICE_TEST_PG`), and the new-code
  coverage gate is met on the always-run SQLite path.

### Definition of Done
- New `#[cfg(test)]` cases in `sqlite.rs` (load nodes/edges round-trip, removed-node absent) and
  `graph.rs` (`from_records` snapshot-equivalence-vs-parsed-baseline, single-file no-spurious-diff,
  multi-file no-cross-file-removal); `just test` green; new-code coverage ≥ 90% on the SQLite + graph
  paths.
- `just qg` clean (fmt-check + clippy `-D warnings` + test).
- Doc comments on the two new trait methods and `from_records` (incl. the `child_ids` re-derivation
  and "roster not restored, reconciled by activity" notes); `storage/mod.rs` and `graph.rs` module
  docs re-checked per `AGENT_PROTOCOL.md §6`.

## Story P9-2: Crash-rebuild — rehydrate the graph from the DB at startup (backend)

Wire P9-1 into boot. In `run_with_db_url` (`app.rs:189-283`), after `open_store` + `ensure_schema`
succeed and **before** `serve` (`app.rs:254`) and before the persist task takes the store
(`app.rs:218`): `load_nodes(RUN_SESSION_ID)`/`load_edges(RUN_SESSION_ID)` — keyed on the
`RUN_SESSION_ID = "sess-local"` constant (`app.rs:55`), **not** a most-recent-session lookup (Design
Decision #2) — build the graph via `Graph::from_records`, then let the existing WalkDir re-parse
reconcile (Design Decision #1). Resolve the store-ownership issue (read before spawn, or make the
handle `Arc<dyn Storage + Send + Sync>`). Any read failure logs and falls back to today's
empty-then-parse path.

### Depends On: P9-1
### Touches: crates/backend/src/app.rs

### Acceptance Criteria
- With `LATTICE_DB_URL` set to a file SQLite DB pre-seeded with persisted nodes/edges for
  `RUN_SESSION_ID`, a fresh `run_with_db_url` produces a **non-empty** first `snapshot` (served by
  `ws.rs:135`) reflecting the persisted roots **before** any file is re-parsed — asserted by an
  integration test that seeds a store, boots the stack against an empty temp repo dir (so re-parse
  adds nothing), connects a WS client, and checks the first snapshot carries the seeded roots.
- **Multi-session regression (adversarial review HIGH):** seed the store with nodes under
  `RUN_SESSION_ID` **and** an additional, later-started `sessions` row (a distinct CLV session
  string, e.g. via a `test.result` persist), then boot — the rehydrate still loads the
  `RUN_SESSION_ID` nodes (proving it keys on the run-session constant, not the most-recent row, which
  would load zero).
- With `LATTICE_DB_URL` **unset**, boot is byte-for-byte today's behaviour (empty graph then
  filesystem parse) — a regression test asserts the no-DB path still yields the root-only snapshot.
- A storage read error during rehydrate is caught, logged, and degrades to the empty-then-parse path
  (the run does not panic or abort) — asserted with a failing/again-empty store stub.
- After rehydrate + reconcile against a repo whose on-disk source differs from the persisted graph,
  the served graph matches the **re-parsed filesystem** (reconcile wins over stale DB), proving the
  warm-start is corrected by re-parse.

### Definition of Done
- New `#[tokio::test(flavor = "multi_thread")]` cases in `app.rs` (rebuild-from-seeded-DB,
  no-DB regression, read-error degradation, reconcile-corrects-drift) mirroring the
  `app.rs:338` full-stack template; `just test` green; new-code coverage ≥ 90%.
- `just qg` clean.
- Doc cascade: the `run_with_db_url` doc updated to describe the rehydrate-then-reconcile boot order;
  `app.rs` module doc re-checked (`AGENT_PROTOCOL.md §6`).

## Story P9-3: `metrics.update` wire contract (backend — the CLV seam)

Add the self-observability envelope to the Rust contract and the spec, with no behaviour yet. Add
`EventType::MetricsUpdate` (`#[serde(rename="metrics.update")]`) to `wire.rs:165-203` and a
`Payload::MetricsUpdate { … }` variant **declared last** (`wire.rs:509`) carrying the all-integer
fields from Design Decision #3, plus a `FileParseLatency` struct; every camelCase field gets an
explicit `#[serde(rename=…)]` (the untagged enum has no container rename). Add a `metrics.update`
row to `DATA_MODEL.md` §A.4 (the `type` list) and §A.5 (a json example). Because the persist match in
both backends is exhaustive with no wildcard, add a no-op skip arm `Payload::MetricsUpdate { .. } =>
{}` to `sqlite.rs` and `postgres.rs` (metrics are ephemeral — `DATA_MODEL.md` §B.5, no DDL table).

### Depends On: none
### Touches: crates/backend/src/wire.rs, crates/backend/src/storage/sqlite.rs, crates/backend/src/storage/postgres.rs, docs/orignal_specs/DATA_MODEL.md

### Acceptance Criteria
- `Payload::MetricsUpdate` carries `sessionId:String`, `ts:String`, `nodeCount:u64`,
  `edgeCount:u64`, `memoryBytes:u64`, `eventsPerSecMilli:u64`, and
  `parseLatency: Vec<FileParseLatency>` where `FileParseLatency { filePath:String, durationUs:u64 }`;
  all camelCase JSON keys via explicit serde rename. `Payload` and `EventEnvelope` **still derive
  `Eq`**, which requires `FileParseLatency` to **also `derive(Eq)`** (its `String`+`u64` fields allow
  it) since `Payload::MetricsUpdate` holds `Vec<FileParseLatency>` — `just qg` compiles clean.
- `serde_json` round-trips a `metrics.update` envelope (serialize → deserialize → equal); a unit test
  decodes the literal JSON from `DATA_MODEL.md` §A.5 into `Payload::MetricsUpdate`.
- Untagged-decode disambiguation is proven: a `metrics.update` payload does **not** mis-decode as any
  earlier `Payload` variant and no earlier variant's JSON decodes as `MetricsUpdate` (the variant is
  last and gated on `nodeCount`/`parseLatency`).
- Persisting a `metrics.update` envelope is a **no-op** (no row written to any table) — asserted via
  the `temp_store()` table-count helper before/after.
- `protocol_sentinel()` is unchanged (`"#CLV1"`); the type is additive (`v:1`).
- `DATA_MODEL.md` §A.4 `type` list includes `metrics.update` and §A.5 has a matching json example;
  the example decodes in the round-trip test (spec ⇄ code parity).

### Definition of Done
- New `#[cfg(test)]` cases in `wire.rs` (round-trip, literal-JSON decode, untagged disambiguation,
  Eq-preserved) and a `sqlite.rs` no-op-persist case; `just test` green; new-code coverage ≥ 90%
  (the `postgres.rs` no-op arm is coverage-excluded when no Docker Postgres, per the Phase-7/8
  precedent).
- `just qg` clean.
- Doc comments on the new `EventType` variant, `Payload::MetricsUpdate`, and `FileParseLatency`;
  `DATA_MODEL.md` updated as a deliberate, noted additive extension (`AGENT_PROTOCOL.md §6`).

## Story P9-4: Backend metrics emitter + parse-latency instrumentation (backend)

Make the metrics real. Time each parse at the `ingest_file` call site (`app.rs:107-120`, wrapping
`parse_source`) and record `durationUs` per file into a small map on `Graph` (new
`record_parse_latency(path, micros)` + a bounded map keyed by repo-relative path). Add a 4th spawned
task in `run_with_db_url` (alongside the watcher pump / collector, `app.rs:~270`) that ticks on a
`tokio::time::interval`, holds an `events_tx.subscribe()` receiver to **count envelopes per window**
(→ `eventsPerSecMilli`), locks the graph for `nodeCount`/`edgeCount`/`memoryBytes` (deterministic
estimate from map sizes) + the latency map, and broadcasts the envelope. Because `Graph::envelope`/
`envelope_at` (`graph.rs:647,656`) are **private** (adversarial review), expose a new `pub`
`Graph::metrics_envelope(events_per_sec_milli, now_ts)` (or `metrics_payload` + a public wrapper)
that stamps `v`/`ts`/`sessionId` via the private helper. `RunHandle` gains a `metrics_task` handle
and `RunHandle::shutdown` (`app.rs:78-86`) **aborts** it alongside the other tasks.

### Depends On: P9-3
### Touches: crates/backend/src/app.rs, crates/backend/src/graph.rs

### Acceptance Criteria
- `Graph` exposes a deterministic `metrics_payload(events_per_sec_milli, now_ts)` (or equivalent)
  builder that, given a graph with N nodes / M edges and a recorded per-file latency map, returns a
  `Payload::MetricsUpdate` with `nodeCount==N`, `edgeCount==M`, a `memoryBytes` estimate that is a
  pure function of the map sizes (same input → same output), and `parseLatency` listing each recorded
  file's `durationUs` — asserted by a `graph.rs` unit test (no timers, no sleeps).
- `record_parse_latency` stores the most-recent `durationUs` per repo-relative path and is bounded
  (the map does not grow beyond the number of distinct source files; a re-parse of the same path
  overwrites, asserted).
- The emitter task broadcasts a `metrics.update` envelope at least once within a bounded test window;
  an integration test subscribes a broadcast receiver, drives one file parse, and observes a
  `metrics.update` whose `nodeCount`/`edgeCount` match the graph and whose `parseLatency` includes
  the parsed file (use a short test interval injected via the run config / a small constant — no
  wall-clock flakiness; assert presence, not an exact latency value).
- `eventsPerSecMilli` reflects broadcast throughput over the window (a window with K observed
  envelopes yields a proportional non-zero value; a quiet window yields 0) — asserted with a
  controllable interval and a known number of injected events.
- Persistence still works: the emitted `metrics.update` flows through the persist subscriber as a
  no-op (P9-3) and does not error the run.

### Definition of Done
- New `#[cfg(test)]` cases in `graph.rs` (deterministic `metrics_payload`, bounded latency map) and
  `app.rs` (emitter broadcasts metrics, events/sec accounting) using injected intervals/clocks — no
  real sleeps where avoidable; `just test` green; new-code coverage ≥ 90%.
- `just qg` clean.
- Doc cascade: docs on the emitter task, `record_parse_latency`, and `metrics_payload`; `app.rs` and
  `graph.rs` module docs note the new self-observability responsibility (`AGENT_PROTOCOL.md §6`).

## Story P9-5: Frontend metrics ingest + debug panel (TS)

Mirror the P9-3 contract on the TS side and render it. Add `MetricsUpdatePayload` +
`FileParseLatency` interfaces and the `metrics.update` arm to the `EventEnvelope` union
(`types.ts:230-241`); add `'metrics.update'` to `KNOWN_EVENT_TYPES` (`ws.ts:171-183`) — **this is
what stops `parseEnvelope` dropping the type** at `ws.ts:257` — and add a `case 'metrics.update'`
shape-check to `isValidPayload` (`ws.ts:212-234`), which **rejects malformed** metrics payloads
(`isValidPayload` ends `default: return true`, so without the case a malformed payload would be
wrongly accepted — adversarial review). Add an `applyEvent` branch (`ws.ts:78`) that stores the
latest metrics in `GraphState` (a `metrics: MetricsUpdatePayload | null` field, seeded by
`initialState`) — reducer stays pure; note that adding the `GraphState` field forces `metrics:
state.metrics` to be threaded through **every** `applyEvent` return site (~11 branches; TS's
missing-property check enforces this, so it is safe but not a one-liner). Expose a derived `metrics`
store. Add `MetricsPanel.svelte` (copy `RosterPanel.svelte`'s
`w-72 shrink-0` accessible aside + `role="status" aria-live="polite"`) showing node/edge counts,
memory, events/sec, and top parse latencies; gate it behind a `showMetrics` `$state(false)` toggle
with a checkbox in the `Graph.svelte` `<fieldset>` (`Graph.svelte:197-215`), mounted beside
`RosterPanel`/`Sidebar`.

### Depends On: P9-3, P9-4
### Touches: frontend/src/lib/types.ts, frontend/src/lib/ws.ts, frontend/src/lib/ws.test.ts, frontend/src/lib/MetricsPanel.svelte, frontend/src/lib/MetricsPanel.test.ts, frontend/src/lib/Graph.svelte

### Acceptance Criteria
- `parseEnvelope` accepts a well-formed `metrics.update` message (matching the P9-3 JSON) and returns
  a typed envelope; a malformed payload (missing `nodeCount`/`parseLatency`, or `parseLatency` not an
  array of `{filePath,durationUs}`) returns `null` (never throws, never widens to `any`).
- `applyEvent` folding a `metrics.update` sets `GraphState.metrics` to the payload; a later
  `metrics.update` replaces it; the reducer returns fresh state and never mutates the input (purity
  asserted). `EventType` (`EventEnvelope['type']`) includes `'metrics.update'` and `npm run check` is
  clean (the exhaustive `switch` compiles only because the branch was added).
- `MetricsPanel` renders `nodeCount`, `edgeCount`, a human-readable `memoryBytes`, an events/sec
  derived from `eventsPerSecMilli/1000`, and the parse-latency list; an updated metrics event updates
  the rendered values (component test via `@testing-library/svelte`).
- The `showMetrics` toggle mounts/unmounts `MetricsPanel` (asserted via the rendered DOM / testid),
  parity with the existing `showAgents` pattern.

### Definition of Done
- New vitest cases in `ws.test.ts` (parse accept/reject, metrics fold, purity, derived store) and
  `MetricsPanel.test.ts` (render, update, toggle); `npm test` green; new-code coverage ≥ 90%.
- `npm run check` (svelte-check strict) zero errors; `npm run lint` (prettier) clean.
- **UI validated in the running app** per CLAUDE.md: `just run`, load http://localhost:5173, toggle
  the metrics panel, confirm live counts/latency update as files change, screenshot before/after via
  the Claude-in-Chrome MCP, and confirm no console errors.
- TSDoc on the new payload/interfaces and reducer branch; `MetricsPanel` prop docs; the
  `frontend/README.md` event-flow section re-checked for the metrics path (`AGENT_PROTOCOL.md §6`).

## Story P9-6: Frontend WS reconnect + backoff + resync + bounded-memory regression (TS)

Give the client a resilient socket lifecycle. Today `connect()` registers only a `'message'`
listener (`ws.ts:304-312`) and `+page.svelte:16-17` captures `client.socket` **once**, so a naive
reconnect would leave the app sending expands to a **dead socket** (adversarial review CRITICAL).
This story therefore changes the client shape, not just adds a loop:
1. **Stable client handle.** `ws.ts` reconnect wraps the raw socket in a loop: on `'close'`/`'error'`
   retry with **exponential backoff + cap + jitter**; on re-`'open'` send the `{"type":"snapshot"}`
   resync frame then re-issue `expand` for every still-open node. The returned `WsClient` exposes a
   stable `requestExpand(nodeId)`/`send` that **always targets the current live socket** (so callers
   never hold a stale socket); `+page.svelte`/`Graph.svelte` call expansions through that handle
   instead of a one-shot `socket` prop (`Graph.svelte:156`).
2. **Open-node set crosses the layer.** The reconnect handler needs the set of open nodes, which is
   `expanded` — component-local `$state` in `Graph.svelte:80`. Expose it to the handler via a
   callback the client invokes on re-open (or a shared store), so re-expand uses the real open set.
3. **Connection-status store** (`writable` in `ws.ts`: `'connecting'|'open'|'reconnecting'|'closed'`)
   drives a small "reconnecting" indicator in the UI.
Keep the pure-reducer / impure-socket split (`ingest` stays the single mutation entry,
`ws.ts:286-288`). Also **regression-lock bounded memory**: assert an expand→collapse cycle returns
the store to baseline and leaks no orphan nodes (collapse follows `parentId`, `ws.ts:343-364`). The
"stale `expanded`" gotcha is a **call-site/state-ownership** fix (`collapse` is a pure
`GraphState→GraphState` reducer with no `expanded` field — keep `expanded` in sync wherever the new
reconnect code runs collapse, not inside `collapse`).

### Depends On: P9-7
### Touches: frontend/src/lib/ws.ts, frontend/src/lib/ws.test.ts, frontend/src/lib/Graph.svelte, frontend/src/routes/+page.svelte

### Acceptance Criteria
- Using an **extended** `MockSocket` (`ws.test.ts:176-204` — add a `sends: string[]` recorder, a
  `readyState`/`OPEN`, and a static registry of constructed instances so the test can grab the
  post-reconnect socket; emit close via `sock.emit('close', …)` since `close()` does not fire the
  event) stubbed via `vi.stubGlobal('WebSocket', …)`: a `'close'` triggers a reconnect attempt after
  a backoff delay; consecutive failures increase the delay (exponential) up to a cap; a successful
  re-open **resets** the backoff. Timers driven by `vi.useFakeTimers()` (no real waits).
- On re-open the client sends a `{"type":"snapshot"}` frame **and** one
  `{"type":"expand","nodeId":…}` per currently-open node — asserted via the **post-reconnect**
  socket's recorded `sends`. After the test mock-server replays a `snapshot` (+ an `agent.roster`
  trailer, P9-7) and the re-requested `subtree`s, `GraphState` (including `agents`) equals the
  pre-disconnect state (BUILD_PLAN cross-cutting: "a graph identical to the server's state").
- After a reconnect, a **user-initiated** expand through the `WsClient` handle goes to the **new**
  live socket (recorded on the post-reconnect instance), proving no stale-socket leak.
- The connection-status store transitions `connecting → open`, `open → reconnecting` on drop, and
  back to `open` on recovery; the UI shows a "reconnecting" indicator while disconnected (asserted in
  a component test).
- Bounded-memory regression: after `expand(file) → expand(fn) → collapse(file)` the `nodes`/`edges`
  Map sizes return to the pre-expand baseline with no orphaned descendant ids; repeating the cycle N
  times does not grow the store beyond baseline (asserted on Map sizes). The reconnect path keeps
  `expanded` consistent with the store (no stale `expanded` entry for a collapsed id).

### Definition of Done
- New vitest cases in `ws.test.ts` (backoff schedule, reset-on-success, resync+re-expand sends on the
  post-reconnect socket, state-identity incl. roster after reconnect, no-stale-socket, status-store
  transitions, expand/collapse memory bound) using fake timers + the extended `MockSocket`; `npm
  test` green; new-code coverage ≥ 90%.
- `npm run check` zero errors; `npm run lint` clean.
- **UI validated** per CLAUDE.md: `just run`, kill/restart the backend, confirm the client shows
  "reconnecting" then resyncs to an identical graph (incl. roster) with no console errors;
  before/after screenshots via the Claude-in-Chrome MCP.
- TSDoc on the reconnect loop, backoff calc, and status store; `ws.ts` module doc updated to remove
  the "no reconnection (Phase 9)" note and describe the lifecycle (`AGENT_PROTOCOL.md §6`).

## Story P9-7: Snapshot / resync carries roster state (backend — deferral #3)

Close the deferred Phase-8 finding: a client connecting or resyncing after agent activity sees agent
nodes but an empty roster. Per Design Decision #4, in `handle_connection` (`ws.rs:126-174`) send the
root-only snapshot (`ws.rs:135`) **then**, when the `Graph` roster is non-empty, send an
`agent.roster` envelope built from `Graph.roster` (expose a `Graph::roster_snapshot()` accessor that
returns the current `agent.roster` envelope, reusing `roster_envelope`, `graph.rs:637`). Apply the
same on the `{"type":"snapshot"}` resync branch (`ws.rs:155-159`). No `Payload::Snapshot` schema
change.

### Depends On: none
### Touches: crates/backend/src/ws.rs, crates/backend/src/graph.rs

### Acceptance Criteria
- `Graph` exposes a method returning the current roster as an `Option<EventEnvelope>` (an
  `agent.roster` payload over the live `roster` map; `None` when the roster is empty) — asserted by a
  `graph.rs` unit test seeding the roster (via the Phase-8 activity path) and checking the envelope's
  agents.
- On connect, a client whose graph has a non-empty roster receives **two** ordered messages: the
  `snapshot` first, then an `agent.roster` carrying every roster entry — asserted by a `ws.rs`
  integration test (seed roster, connect, read first two envelopes).
- A client connecting against an **empty** roster receives the snapshot and **no** trailing roster
  envelope (no spurious empty roster) — regression-asserted so the existing first-message-is-snapshot
  tests (`ws.rs:280`) still hold.
- The same roster-after-snapshot behaviour fires on a `{"type":"snapshot"}` resync request.

### Definition of Done
- New `#[cfg(test)]` cases in `graph.rs` (roster_snapshot present/empty) and `ws.rs` (connect →
  snapshot+roster, empty-roster no-trailer, resync → snapshot+roster); `just test` green; new-code
  coverage ≥ 90%.
- `just qg` clean.
- Doc comments on the roster accessor + the updated connect/resync flow; `ws.rs` module doc
  re-checked (`AGENT_PROTOCOL.md §6`).

## Story P9-8: Bounded backend memory — roster retention sweep + hot-edge tallies eviction + length-bounded collector read (backend — deferrals #1, #2)

Cap the three unbounded backend buffers recon found. (a) **Roster retention** (deferral #1):
`Graph.roster`/`Graph.last_seen` are insert-only — `expire_idle` flips to `inactive` but never
removes. Add a longer **retention window** and a sweep that reclaims a `process_id` from **both**
maps once it has been `inactive` beyond that window, driven from the `collect` loop on every tick
(like `expire_idle`); a reclaimed pid must not resurrect unless new activity arrives. (b) **Hot-edge
tallies eviction** (scope #4 harden): `HotEdgeThrottle.tallies` (`tracing_layer.rs:117-121`) is never
evicted — drop tally entries whose `window_id` is older than the current window via an **amortized
sweep triggered when `tallies.len()` exceeds a cap** (not an O(N) scan inside the zero-alloc `note`
hot path, `tracing_layer.rs:153-168`), keeping the per-edge throttle correctness. (c) **Length-bounded
collector read** (deferral #2): `poll_once` (`collector.rs:147-197`, read body L174-196) `read_to_end`s the **entire**
appended region in one allocation and accumulates a partial line across polls if no `\n` arrives —
bound **both**: read in capped chunks (not `read_to_end`) and, on an over-length line, **resync to the
next `\n`** (carrying a skip flag across polls when the over-long line spans reads), never corrupting
`#CLV1` parsing of the following line.

### Depends On: none
### Touches: crates/backend/src/graph.rs, crates/backend/src/collector.rs, crates/backend/src/tracing_layer.rs

### Acceptance Criteria
- A named retention-window constant **strictly greater than the Phase-8 idle window**
  (`ROSTER_IDLE_MS = 5_000`, `graph.rs:69`) exists; a `Graph` method (e.g. `reclaim_inactive(now)`)
  removes from **both** `roster` and `last_seen` every process whose `last_seen` is older than the
  retention window **and** whose status is `inactive`, leaving active and recently-inactive processes
  intact — asserted against a controllable `now` (no real sleeps). Reclaim lives **only** on the
  collector tick, **not** inside `expire_idle`, so the merged P8-4 graph tests (which call
  `expire_idle` directly, `graph.rs:1612-1641`) are untouched and the respawn AC (inactive pid stays
  visible at age 5001ms) still holds.
- The collector drives `reclaim_inactive` from the `collect` loop on every tick (right after
  `expire_idle`, `collector.rs:132-136`; not behind `poll_once`'s no-growth early return), so a quiet
  sink still reclaims long-idle pids; after reclamation the roster no longer lists the pid, and a new
  activity for a fresh pid is unaffected (asserted via an injected clock).
- `HotEdgeThrottle` bounds `tallies` via an **amortized sweep when the map exceeds a named cap**
  (dropping entries older than the current `window_id`); a test feeding transitions across many
  distinct edge ids over many windows asserts the map **does not grow without bound** (stays ≤ the
  cap + one window's live edges), while `throttle_bounds_emissions_per_window` (`tracing_layer.rs:351`)
  still passes (eviction is correctness-neutral — `note` re-freshes any re-touched edge,
  `tracing_layer.rs:173-175`).
- `poll_once` reads in **capped chunks** (not `read_to_end`); a line exceeding a named max-line
  constant is dropped by **resyncing to the next `\n`** (with a skip flag carried across polls when
  the over-long line spans reads), while a normal `#CLV1` line is still parsed and forwarded —
  asserted by feeding an over-long line **split across two polls** followed by a valid line, checking
  only the valid one is broadcast and the parser is not corrupted.

### Definition of Done
- New `#[cfg(test)]` cases (roster reclaim flip+remove, quiet-sink reclaim, throttle map bounded
  under many-edges-many-windows, over-long-line resync across two polls) with deterministic clocks —
  no real sleeps; `just test` green; new-code coverage ≥ 90%.
- `just qg` clean.
- Doc comments on the retention constant + `reclaim_inactive`, the tallies-eviction note (replacing
  the "finite edge set" assumption at `tracing_layer.rs:117-121`), and the collector max-line bound;
  module docs re-checked (`AGENT_PROTOCOL.md §6`).

## Story P9-9: Watcher debounce hardening — distinct-path coverage + starvation cap (backend)

Harden the debounce against the liveness gap recon found. The window *extends* on every event
(`watcher.rs:57-60`) → a file written continuously every <150ms is never flushed (unbounded
latency). Add a **maximum debounce cap** by restructuring the single `timeout(DEBOUNCE, rx.recv())`
(`watcher.rs:57`) into a `tokio::select!` over (next event / quiet-timer / **max-deadline since the
burst's first event**): even under sustained churn, a path is flushed once a named max-wait elapses,
then the window restarts. Add the missing **distinct-paths-in-one-burst** test (the `HashSet`
forwards each distinct path once). Keep the existing same-path coalescing behaviour and tests
(`watcher.rs:137,178,216`) green.

### Depends On: none
### Touches: crates/backend/src/watcher.rs

### Acceptance Criteria
- A named max-debounce constant exists; `debounce_loop` flushes a continuously-touched path within
  the max-wait even while events keep arriving — asserted by a **current-thread
  `#[tokio::test(start_paused = true)]`** driving `debounce_loop` **directly** (it waits on tokio's
  own timers, which paused time auto-advances deterministically; the multi-thread `notify` tests at
  `watcher.rs:178,216` depend on real filesystem events and stay wall-clock — do **not** pause them).
  The test sends one path every interval shorter than `DEBOUNCE` past the max-wait and asserts at
  least one emission occurs before the churn stops.
- Existing same-path coalescing is preserved: a 3-send burst of one path that then goes quiet still
  yields exactly one emission (regression — the `watcher.rs:137` test still passes unchanged).
- A burst containing several **distinct** source paths flushes each path exactly once after the
  window quiets (new test) — order-independent set assertion.
- `is_source_file` behaviour is unchanged (`rs|py|ts` only; `a.rs.bak` rejected — `watcher.rs:133`
  regression holds).

### Definition of Done
- New `#[tokio::test]` cases (max-wait flush under sustained churn, distinct-paths fan-out) plus the
  preserved same-path regression; deterministic timing via `tokio::time` control where possible;
  `just test` green; new-code coverage ≥ 90%.
- `just qg` clean.
- Doc comments on the max-debounce constant + the extend-with-cap rationale; `watcher.rs` module doc
  re-checked (`AGENT_PROTOCOL.md §6`).

## Story P9-10: Malformed-code graceful degradation — regression-lock + document the `syn` limitation (backend)

Lock the "never crash on bad syntax" invariant across all paths (non-destructively — Design Decision
#5). Add a **fuzz/property panic-freedom test** that feeds a deterministic corpus of malformed/random
byte strings (and the non-source + empty-input fallback) through `parse_source` and asserts no panic
(wrapping in `std::panic::catch_unwind`, the established pattern at `mod.rs:1082`). Add the missing
**sibling-recovery** test for the tree-sitter paths: a Python/TS file with a valid function **placed
first** and a broken function after it still emits the valid function's node (siblings live, already
true per `treesitter.rs:370-438`), with a file-level `NodeStatus::Error`. Document the Rust/`syn`
all-or-nothing reality **in the parser module docs only** — **do not touch `SPEC.md`,
`BUILD_PLAN.md`, or `CLAUDE.md`** (the "offending node marked `error`" contract stays intact; fully
meeting it, or formally amending the spec, is a user-directed follow-up — Design Decision #5).

### Depends On: none
### Touches: crates/backend/src/parser/mod.rs, crates/backend/src/parser/treesitter.rs

### Acceptance Criteria
- A property/fuzz test feeds a deterministic corpus of malformed/random inputs (seeded — fixed byte
  vectors + a bounded generator, no nondeterministic RNG) to `parse_source` for the `rs`, `py`, `ts`,
  an unknown extension, and empty source, asserting **no path panics** and each returns at least a
  file node (the unknown/empty path yields the bare `file` node, `mod.rs:193-204`).
- A Python source with a valid `def good()` **first** and a malformed `def (:` **after** still emits
  a node for `good` (sibling recovery — valid-first so tree-sitter's ERROR region cannot swallow it)
  and a file-level `NodeStatus::Error` (`treesitter.rs:335-339`) — asserted by id presence; the same
  for an equivalent TypeScript case (valid function first).
- The Rust/`syn` all-or-nothing behaviour is regression-asserted (a syntax error yields **only** the
  file node `NodeStatus::Error`, no function/variable nodes — `mod.rs:104-113`), documenting the
  language asymmetry as intended.
- No product-spec or `CLAUDE.md` file is modified by this story (verified by the diff touching only
  `parser/mod.rs` + `parser/treesitter.rs`); the `syn` limitation is recorded as a parser
  module-doc note.

### Definition of Done
- New `#[cfg(test)]` cases in `parser/mod.rs` (dispatch fuzz/property panic-freedom, Rust
  all-or-nothing regression) and `parser/treesitter.rs` (Python + TS valid-first sibling recovery);
  `just test` green; new-code coverage ≥ 90%.
- `just qg` clean.
- Parser module docs (`parser/mod.rs`, `treesitter.rs`) updated so the recovery contract reads
  accurately — including the `syn` all-or-nothing note and the tree-sitter sibling-recovery behaviour
  (`AGENT_PROTOCOL.md §6`); no spec/CLAUDE.md edits.

---

## Dependency graph

```
P9-1  Storage read methods + Graph::from_records (storage/*, graph.rs) ........... Depends: none
  └─ P9-2  Crash-rebuild wired into startup (app.rs) ............................. Depends: P9-1
P9-3  metrics.update wire contract (wire.rs, storage/*, DATA_MODEL.md) ........... Depends: none
  └─ P9-4  Backend metrics emitter + parse-latency (app.rs, graph.rs) ........... Depends: P9-3
       └─ P9-5  Frontend metrics ingest + panel (types.ts, ws.ts, MetricsPanel) . Depends: P9-3, P9-4
P9-7  Snapshot/resync carries roster (ws.rs, graph.rs) .......................... Depends: none
  └─ P9-6  Frontend WS reconnect + backoff + resync + memory regression (ws.ts) . Depends: P9-7
P9-8  Bounded backend memory: roster reclaim + throttle evict + collector cap ... Depends: none
P9-9  Watcher debounce hardening + starvation cap (watcher.rs) .................. Depends: none
P9-10 Malformed-code regression-lock + doc syn limit (parser/*) ................. Depends: none
```

- **Acyclic.** Three logical chains: crash-rebuild (P9-1 → P9-2), metrics (P9-3 → P9-4 → P9-5), and
  resync (P9-7 → P9-6, so the frontend reconnect can verify roster-carrying resync end-to-end). The
  rest are independent hardening/regression stories.
- **Serialization by overlapping `Touches` (no explicit edge needed):** `graph.rs` is edited by
  P9-1, P9-4, P9-7, P9-8 on disjoint sections — the shared glob makes the scheduler run them one at a
  time (Design Decision #6). `app.rs` (P9-2, P9-4), `storage/sqlite.rs`+`postgres.rs` (P9-1, P9-3),
  and `ws.ts`+`ws.test.ts`+`Graph.svelte` (P9-5, P9-6) likewise serialize on overlap.
- **Stack split:** P9-5 and P9-6 are TypeScript (frontend crew); the rest are Rust (backend crew).
- **Phase-9 accept (BUILD_PLAN) is met when:** P9-2 (kill-and-restart rebuilds the graph), P9-6 (the
  client resyncs to an identical graph after a forced disconnect) and P9-10 (malformed code degrades
  gracefully) are merged; P9-7 makes the resync carry roster; P9-4/P9-5 deliver the self-observability
  panel; P9-8/P9-9 bound memory and debounce latency under sustained load.
