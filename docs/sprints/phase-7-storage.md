# Lattice Phase 7 — Storage abstraction

Adds **persistence**: structured CLV events and graph state are written through a single `sqlx`-based
`Storage` trait to one of two interchangeable backends — **SQLite** (solo/local) or **Postgres**
(team) — selected by `LATTICE_DB_URL`. Today the backend is entirely in-memory: `app::run` builds a
`Graph` + a `broadcast::Sender<EventEnvelope>` and never touches a database (verified: zero `sqlx` /
`LATTICE_DB_URL` / `storage` hits in `crates/backend`). Phase 7 makes a run durably record what
happened **without changing the in-memory serving path** — the `Graph` stays the source of truth for
snapshots/subtrees; storage is write-through persistence of the structured `EventEnvelope` stream.
System-level **done** (BUILD_PLAN.md Phase 7 "Accept when"): the same run persists identically to
local SQLite and a Docker Postgres with no code change; only structured CLV events are written (raw
stdout is ephemeral, never persisted).

**Grounding (read this session; Phases 0–6 merged on `main` at `63131b5`).**
- `crates/backend/Cargo.toml` — `[dependencies]` currently: serde, serde_json, syn, proc-macro2,
  quote, tree-sitter(+python+typescript), notify, tokio (rt/rt-multi-thread/macros/sync/time/net/
  io-util/fs/signal), tokio-tungstenite, futures-util, walkdir, tracing, tracing-subscriber;
  `[dev-dependencies]` tempfile. **No `sqlx`.**
- `crates/backend/src/app.rs` — `run(root, addr) -> io::Result<RunHandle>` (app.rs:127) canonicalises
  `root`, builds `let (events_tx, _) = broadcast::channel::<EventEnvelope>(EVENT_CHANNEL_CAPACITY)`
  (app.rs:130, capacity 1024), initial-parses via `WalkDir`, `serve`s (app.rs:140), then spawns three
  tasks: `watcher_task` + `pump_task` (re-parse → `events_tx.send(event)`, app.rs:149-155) and
  `collector_task` (`collect(root, graph, events_tx)`, app.rs:159). `RunHandle` (app.rs:45-52) holds
  `addr`, `server`, `watcher_task`, `pump_task`, `collector_task`; `RunHandle::shutdown` (app.rs:57)
  aborts all three. **This is the wiring point** for a persistence task: `events_tx.subscribe()` and
  persist each structured `EventEnvelope`.
- `crates/backend/src/wire.rs` — `EventEnvelope { v: u8, ts: String, session_id: String, event_type:
  EventType, payload: Payload }` (the only thing that flows on `events_tx`). `Payload` variants carry
  the structured data: `TestResult { node_id, test_id, outcome: TestOutcome, duration_ms: Option<u64>,
  session_id, agent_id: Option<String>, process_id: Option<u32>, message: Option<String> }`,
  `StatusUpdate { node_id, status: NodeStatus, session_id, agent_id, process_id }`, `HotEdge {
  edge_id, state: HotEdgeState, process_id: Option<u32>, session_id, agent_id, ts }`, `NodeUpsert {
  node: Node }`, `EdgeUpsert { edge: Edge }`, `NodeRemove { id }`, `EdgeRemove { id }`, plus
  `Snapshot`/`Subtree` (server→client only — NOT persisted). `Node` carries `id, type, label,
  parent_id, status, docs, signature, meta`; `Edge` carries `id, source, target, kind, hot`. `process_id`
  is `Option<u32>` on the wire.
- `crates/backend/src/collector.rs` — tails `<root>/.lattice/clv.ndjson`; **only** `parse_clv_line`
  output (structured `ClvEvent` → patch `EventEnvelope`) reaches `events_tx`; untagged/malformed lines
  parse to `None` and are dropped (collector.rs:143). So subscribing to `events_tx` already means
  "structured events only" — raw stdout never becomes an envelope.
- `crates/backend/src/graph.rs` / `lib.rs` — the in-memory `Graph` is the live source of truth and
  **stays so** (Phase 7 persists alongside; rebuild-from-DB on crash is Phase 9, not here). `lib.rs`
  (`pub mod app/clv/collector/graph/parser/tracing_layer/watcher/wire/ws`) is where `pub mod storage;`
  is declared.

**Contracts — DATA_MODEL.md Part B is the source of truth (do not invent schema).**
- §B.6 DDL sketch — seven tables with these exact column sets: `sessions(session_id PK, started_at,
  repo_path, label)`; `agents(process_id PK, agent_id, agent_type, color, status, session_id FK,
  created_at, updated_at)`; `protocol_versions(process_id PK FK, version, session_id FK, introduced_at,
  deprecated_at NULL, features_json)`; `nodes(id PK, session_id FK, type, label, parent_id, status,
  docs, signature_json, meta_json, last_process_id, last_agent_id, updated_at)`; `edges(id PK,
  session_id FK, source, target, kind, hot)`; `test_results(id PK, process_id FK, session_id FK,
  node_id, test_id, outcome, duration_ms, agent_id, message, ts)`; `agent_activity(id PK, process_id
  FK, session_id FK, agent_id, action, node_id, ts)`.
- §B.1 — `process_id` keying: PRIMARY KEY on `agents`/`protocol_versions`; FK on `test_results`/
  `agent_activity`; structural tables (`nodes`/`edges`) keep deterministic structural IDs as PKs and
  carry `process_id`/`agent_id` as attribution columns. §B.5 — ONLY structured CLV events persisted;
  raw stdout ephemeral. §B.7 — indexes: `nodes(parent_id)`, `nodes(session_id,type)`, `edges(source)`,
  `edges(target)`, `edges(kind)`, `agents(session_id,status)`, `test_results(node_id,ts)`,
  `agent_activity(node_id,ts)`. §B.6 builder note: `process_id` is `BIGINT`.

**Key design decisions (resolved here, grounded; CLAUDE.md "extend the contract and note it").**
1. **Dual backend via one async trait, runtime queries.** A `Storage` async trait with two impls —
   `SqliteStore` (holds a `sqlx::SqlitePool`) and `PostgresStore` (holds a `sqlx::PgPool`) — chosen at
   startup by the `LATTICE_DB_URL` scheme. Use the **runtime** `sqlx::query(sql).bind(..).execute(..)`
   API (NOT the compile-time `query!` macro) so **no `DATABASE_URL` is needed at build time** and
   `just qg`/clippy stay hermetic with no database. Per-dialect SQL strings (placeholders `?` for
   SQLite, `$1..` for Postgres). `sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite",
   "postgres"] }`.
2. **Schema via idempotent `CREATE TABLE IF NOT EXISTS` at startup**, per-dialect (no `sqlx::migrate!`
   directory — two dialects make hand-written per-backend DDL the simplest correct path). `ensure_schema`
   is part of the trait and is called once on open.
3. **Timestamps as TEXT rfc3339** in both backends (a deliberate, noted extension of §B.6's `TIMESTAMP`
   type): the codebase already mints rfc3339 strings (`rfc3339_now`), and a TEXT column stores them
   identically in SQLite and Postgres with no `chrono`/`time` dependency and byte-identical parity.
   `process_id` (`Option<u32>` on the wire) widens to `i64` for the `BIGINT` column.
4. **What each event persists, and how §B.6 is reconciled with the real wire data** (the in-memory
   `Graph` is unchanged; these are the write-through rules). The §B.6 DDL assumes a `pid` and full agent
   metadata on every event, but the actual Phase-5/6 wire payloads frequently omit them, so the
   following are **deliberate, noted §B.6 extensions** (CLAUDE.md "extend the contract and note it"):
   - **Missing `pid`:** `process_id` is `Option<u32>` on the wire (`None` whenever a CLV line omits
     `pid` — exercised by the collector's own pid-less test lines). So `test_results.process_id` and
     `agent_activity.process_id` are made **NULLable** (relaxing §B.6's `NOT NULL`); the `agents` row is
     upserted **only when `pid` is `Some`** (a pid-less event persists with `process_id = NULL` and no
     `agents` row). A NULL FK is unconstrained, so §B.6's `REFERENCES agents(process_id)` still holds for
     the `Some` case. When `Some`, `process_id` widens to `i64` for the `BIGINT` column.
   - **Missing `agent_type`/`color`:** no persisted payload carries them (only the not-persisted
     `agent.roster` does). On an `agents` upsert (from a `test.result`/`status.update`/`hot_edge` event
     that carries a `pid` — these are the events that actually reach `events_tx`; see the agent.activity
     note below), `agent_type` defaults to the event's `agent` id (or `"unknown"` when absent) and `color`
     to a deterministic placeholder (`"#888888"`), satisfying the `NOT NULL` columns; the Phase-8 roster
     (which carries the real values) refines them later.
   - **`agent.activity` / `protocol_versions` are NOT written in Phase 7:** there is no
     `Payload::AgentActivity` (nor roster) variant — `Payload` is a closed enum and `Graph::apply_clv`
     maps `ClvEvent::Activity { .. } => None` (graph.rs:317, a Phase-8 no-op) — so no such envelope ever
     reaches `events_tx`. The `agent_activity` and `protocol_versions` **tables are created by
     `ensure_schema`** (per §B.6) but **remain empty in Phase 7**; their write paths land in Phase 8 when
     the agent layer adds the payload variant + an `apply_clv` arm. Phase 7 populates the `agents` table
     only via the pid-carrying `test.result`/`status.update`/`hot_edge` events above.
   - **Event-log row id:** `test_results.id` is `TEXT PRIMARY KEY` with no wire source, so it is minted
     as a **UUID v4** (`uuid` crate, added in P7-1); two test results get distinct ids. (`agent_activity.id`
     is likewise a UUID v4, but only once its writer lands in Phase 8 — see the note above.)
   - **session_id:** event rows reference the **event's own** `session_id` (the payload-level CLV
     session, per §B.2's concurrent-session grouping — NOT the run-level envelope session). To satisfy
     the `REFERENCES sessions(session_id)` FK, a `sessions` row is **upserted per distinct session_id on
     first sight** (lazy), in addition to the run session recorded at open. Per-event insert order:
     upsert `sessions` → upsert `agents` (when pid `Some`) → insert the event row.
   - **`hot_edge`:** persisted **only** as the `edges.hot` boolean — §B.6 has no hot-edge table. This
     **intentionally diverges** from §B.1/§B.5's "hot-edge records" (no per-event history, no
     `process_id` attribution for hot edges); the `edges.hot` UPDATE is a no-op when the edge row is not
     yet persisted (edges are written via `edge.upsert` from the parser, which precedes runtime hot
     events). Noted explicitly.
   - The rest: `status.update` → `nodes.status` UPDATE; `node.upsert`/`node.remove` → `nodes`
     upsert/delete; `edge.upsert`/`edge.remove` → `edges` upsert/delete; `snapshot`/`subtree` → **not
     persisted** (server→client view frames). A `sessions` row is also written once when the store opens
     (the run's repo_path session).
5. **FK enforcement parity:** `SqliteStore` runs `PRAGMA foreign_keys = ON` on every connection so SQLite
   enforces the §B.6 `REFERENCES` constraints **identically to Postgres** (which always does). Without
   this an FK-violating row inserts silently under SQLite but is rejected under Postgres, breaking the
   "persists identically" acceptance. The parity test asserts an FK-violating insert fails under **both**
   backends.

**Docker/Postgres reality (gates the acceptance test).** The Docker daemon was **down** at planning time
(Docker.app is installed and was launched, booting). The Postgres-parity acceptance criterion needs a
running Postgres, so the plan is structured so **`just qg` is fully green with NO database/daemon**: the
SQLite path is file-based and tested live against a tempfile DB; the Postgres path is code-complete with
its live integration/parity test **gated behind Postgres availability** (an `#[ignore]`-by-default test
run explicitly, plus a `LATTICE_TEST_PG`-env-gated path) so a missing daemon never reds the gate. The
Docker-Postgres parity run is a named acceptance criterion executed when the daemon is up.

**Scope discipline (BUILD_PLAN.md Phase 7).** In scope: the `Storage` trait, the SQLite + Postgres
backends, the per-dialect schema, `LATTICE_DB_URL` config, and a persistence task wired into `app::run`
that writes structured events. The `agents`/`agent_activity`/`protocol_versions` **tables** are created
here (per the schema) even though the agent-layer UI is Phase 8. **Out of scope:** backend-crash
rebuild/replay from the DB and WebSocket reconnect (Phase 9); the agent-layer UI + live roster (Phase 8);
any change to how the in-memory `Graph` serves snapshots/subtrees. Raw stdout is **never** persisted
(asserted by a test). When `LATTICE_DB_URL` is unset, the backend runs exactly as today (no DB, no
error) — persistence is additive and opt-in.

**Commands.** Backend `just qg` (= fmt-check + clippy `-D warnings` + `cargo test --all`) / `just test`;
coverage `cargo llvm-cov` (gate **90%** new code). All gates must pass **without a live database**
(SQLite tempfile tests are fine; Postgres live tests are gated/ignored). Doc-comment cascade
(AGENT_PROTOCOL.md §6) on every touched element. `sqlx` is async (tokio) — trait methods are `async`.
Target branch `main`.

---

## Story P7-1: `Storage` trait + sqlx deps + `LATTICE_DB_URL` scheme selection

Lays the storage foundation with no backend logic yet: adds the `sqlx` dependency, defines the async
`Storage` trait (the persistence contract the two backends implement and `app::run` will call), and the
`LATTICE_DB_URL` → backend-scheme resolver + a `open_store` factory. Builds on the verified facts that
no `sqlx`/storage exists today and that `lib.rs` is the module-declaration site. Adds a new
`crates/backend/src/storage/mod.rs` module declared `pub mod storage;` in `lib.rs`.

### Depends On: none
### Touches: crates/backend/Cargo.toml, crates/backend/src/storage/mod.rs, crates/backend/src/lib.rs

### Acceptance Criteria
- `crates/backend/Cargo.toml` gains `sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite",
  "postgres"] }`, `uuid = { version = "1", features = ["v4"] }` (event-log row PKs, decision-4), and
  `async-trait = "0.1"` (the async `Storage` trait is held as `Box<dyn Storage>`, which native
  async-fn-in-trait does not support on stable Rust); the workspace builds (`cargo build -p
  lattice-backend`) with no `DATABASE_URL` set.
- A public `Storage` trait (annotated `#[async_trait::async_trait]` for dyn-compatibility) exists in
  `storage/mod.rs` with (at minimum) `async fn ensure_schema(&self) -> Result<(), StorageError>` and a
  method to persist one structured event, e.g. `async fn persist(&self, env: &EventEnvelope) -> Result<(),
  StorageError>`, plus an `async fn record_session(&self, session_id: &str, repo_path: &str) -> Result<(),
  StorageError>`. The trait is object-safe (via `#[async_trait]`) and held as `Box<dyn Storage + Send +
  Sync>` (a unit test constructs a trivial in-test impl and stores it in a `Box<dyn Storage>` to prove it).
- A `StorageError` type (wrapping `sqlx::Error` + a config/parse variant) implements `std::error::Error`.
- A `fn backend_for_url(url: &str) -> Result<Backend, StorageError>` (or equivalent) classifies the
  scheme: `sqlite:`/`sqlite://…` → `Backend::Sqlite`, `postgres:`/`postgresql://…` → `Backend::Postgres`,
  any other scheme → an `Err` naming the bad scheme. A table-driven test covers sqlite, postgres,
  postgresql, an unknown scheme (`mysql://…` → Err), and a malformed url.
- An `async fn open_store(url: &str) -> Result<Box<dyn Storage + Send + Sync>, StorageError>` factory is
  declared and dispatches on the scheme (its two arms are filled by P7-2/P7-3; in this story the
  Postgres arm may return a `todo!()`-free "unimplemented backend" `Err` or be `#[cfg(...)]`-stubbed —
  no panic path on a normal call).

### Definition of Done
- New unit tests (scheme table, object-safety) written and green; new-code coverage ≥ 90%.
- `just qg` clean **with no DATABASE_URL / no database** (runtime query API only; no `query!` macro).
- Doc cascade: `Storage`, `StorageError`, `Backend`, `open_store`, `backend_for_url` each carry `///`
  docs citing DATA_MODEL §B; `storage/mod.rs` has a `//!` module doc describing the persistence seam;
  `lib.rs`'s module-level doc lists the new `storage` component.

## Story P7-2: SQLite backend — schema + structured-event persistence (live-tested)

Implements `SqliteStore` against the `Storage` trait from P7-1: opens a `SqlitePool`, creates the full
seven-table schema (DATA_MODEL §B.6, SQLite dialect) idempotently, and persists each structured
`EventEnvelope` to the right row(s) per the §B.5 / decision-4 rules. Fully tested live against a
tempfile / `sqlite::memory:` database (no daemon needed). Fills the SQLite arm of `open_store`.

### Depends On: P7-1
### Touches: crates/backend/src/storage/sqlite.rs, crates/backend/src/storage/mod.rs

### Acceptance Criteria
- `SqliteStore::ensure_schema` creates all seven tables (`sessions, agents, protocol_versions, nodes,
  edges, test_results, agent_activity`) with the §B.6 columns and the §B.7 indexes; a test opens a fresh
  in-memory/tempfile DB, runs `ensure_schema`, and asserts every table and index exists (query
  `sqlite_master`). Running `ensure_schema` twice is a no-op (idempotent — `IF NOT EXISTS`).
- `SqliteStore` opens with `PRAGMA foreign_keys = ON` (decision-5) so the §B.6 `REFERENCES` constraints
  are enforced; a test asserts an FK-violating insert (a `test_results` row whose non-NULL `process_id`
  has no `agents` parent) is rejected.
- Persisting a `test.result` envelope **with a `pid`** writes one `test_results` row whose `node_id`/
  `test_id`/`outcome`/`duration_ms`/`session_id`/`agent_id`/`message` round-trip equal to the payload
  (queried back) with a freshly-minted UUID-v4 `id`; `process_id` is the widened `i64`; and an `agents`
  row for that `process_id` exists (status `active`, `agent_type` defaulted to the event `agent` id or
  `"unknown"`, `color` `"#888888"`). Per-event insert order is sessions → agents → event row.
- Persisting a `test.result` envelope **with no `pid`** (the collector's pid-less line case) succeeds:
  the `test_results` row is written with `process_id = NULL` and **no** `agents` row is created — asserted
  by querying both tables (the §B.6 `NOT NULL`-relaxation from decision-4).
- A `sessions` row is upserted for each **distinct event `session_id`** on first sight (so each event
  row's `session_id` FK resolves); persisting two events with the same session adds only one `sessions`
  row; two events produce two distinct UUID `id`s (asserted).
- A `status.update` updates the target `nodes.status` (and upserts `agents` when its `pid` is present);
  a `hot_edge` updates the target `edges.hot` (a no-op, asserted, when the edge row is absent, and
  upserts `agents` when its `pid` is present); a `node.upsert`/`edge.upsert` inserts-or-updates the
  `nodes`/`edges` row; `node.remove`/`edge.remove` deletes it. Each asserted by querying the DB back.
- The `agent_activity` and `protocol_versions` tables are **created by `ensure_schema` but remain empty
  in Phase 7** (no `Payload::AgentActivity`/roster variant exists — `apply_clv` maps `Activity => None`,
  graph.rs:317): a test persists the full set of structured envelopes the collector + parser produce and
  asserts both tables have zero rows (their writers arrive in Phase 8).
- A `snapshot` and a `subtree` envelope persist **nothing** (view frames) — a test asserts row counts
  are unchanged after applying them.
- **Raw stdout is never persisted:** a test confirms there is no code path from an untagged/raw line to a
  DB write — only an `EventEnvelope` reaches `persist`; persisting the set of structured envelopes the
  collector produces for a mixed input writes rows only for the structured ones.
- `record_session` writes one `sessions` row (`session_id`, `started_at`, `repo_path`).

### Definition of Done
- New live SQLite tests (tempfile/memory) written and green; new-code coverage ≥ 90%.
- `just qg` clean with no external database (SQLite needs no daemon).
- Doc cascade: `SqliteStore` + each method documented (citing §B.6/§B.5); the SQLite-dialect schema SQL
  is commented; `storage/mod.rs` `open_store` SQLite arm documented.

## Story P7-3: Postgres backend — dialect schema + Docker-gated parity test

Implements `PostgresStore` against the same `Storage` trait: a `PgPool`, the seven-table schema in
Postgres dialect (`BIGINT`/`BOOLEAN`/`SERIAL`, `$N` placeholders), and the same persistence mapping as
P7-2. Fills the Postgres arm of `open_store`. Because the acceptance test needs a live Postgres and the
Docker daemon may be down, the live parity integration test is **gated** so `just qg` stays green
without it.

### Depends On: P7-2
### Touches: crates/backend/src/storage/postgres.rs, crates/backend/src/storage/mod.rs

### Acceptance Criteria
- `PostgresStore` implements `Storage` with the identical method surface as `SqliteStore`; its schema SQL
  uses Postgres types (`process_id BIGINT`, `hot BOOLEAN`, `ts`/`*_at` TEXT rfc3339 per decision-3) and
  `$1..$N` bind placeholders; the code compiles into `cargo test --all` (the Postgres driver is a normal
  dependency feature) **with no live Postgres**.
- `open_store("postgres://…")` returns a `PostgresStore` (a unit test asserts the scheme dispatches to the
  Postgres arm without connecting — e.g. by checking `backend_for_url` + that connecting to an
  unreachable host returns a `StorageError`, not a panic).
- A **gated parity integration test** (marked `#[ignore]` AND/OR guarded by a `LATTICE_TEST_PG` env var
  giving a Postgres URL) runs `ensure_schema` + the full persist suite against the live Postgres and
  asserts the persisted rows equal what the SQLite backend produces for the same envelope sequence
  (parity). The test is **skipped/ignored when `LATTICE_TEST_PG` is unset**, so `cargo test --all` /
  `just qg` pass with no daemon. A `log!`/comment records that it was skipped.
- The acceptance run (`LATTICE_TEST_PG=postgres://…  cargo test -- --ignored storage::pg_parity`, against
  a Docker Postgres) passes when a daemon is available — documented in the story as the BUILD_PLAN
  "same run persists identically to SQLite and Docker Postgres" check.
- The Postgres schema applies the same decision-4 reconciliations as SQLite —
  `test_results`/`agent_activity.process_id` NULLable, `agent_type`/`color` defaults, UUID event-log
  ids, lazy `sessions` upsert — and the parity test asserts they behave identically: a pid-less
  `test.result` persists with `process_id = NULL` and no `agents` row under both backends, and an
  FK-violating insert is rejected under **both** (SQLite via `PRAGMA foreign_keys = ON`, Postgres
  natively — decision-5).

### Definition of Done
- Postgres backend code-complete; gated parity test written; new-code coverage ≥ 90% **excluding the
  ignored live-only test body** (note any lines only reachable with a live Postgres so the gate is
  measured on the hermetically-reachable code).
- `just qg` clean with no Postgres/daemon (the live test is ignored/gated).
- Doc cascade: `PostgresStore` + methods documented; the dialect differences vs SQLite noted in-code; the
  Docker/parity run procedure documented in the story's DoD and a module comment.

## Story P7-4: Wire the persistence task into `app::run`

Connects storage to the live pipeline: `run` reads `LATTICE_DB_URL`, opens the store (skipping
persistence entirely when unset), `ensure_schema` + `record_session`, and spawns a `store_task` that
`events_tx.subscribe()`s and persists every structured `EventEnvelope`. `RunHandle` gains the new task
handle and aborts it on shutdown. Builds on the verified `run`/`RunHandle` wiring (app.rs:127-167).

### Depends On: P7-2
### Touches: crates/backend/src/app.rs

### Acceptance Criteria
- `run` resolves `LATTICE_DB_URL` (via a passed value or env): when **set** to a `sqlite:` tempfile URL,
  it opens the store, calls `ensure_schema`, writes a `sessions` row, and spawns a `store_task` (a new
  `JoinHandle` on `RunHandle`); when **unset**, `run` behaves exactly as today — no store, no DB file,
  no error — a test asserts a no-`LATTICE_DB_URL` run still serves and shuts down cleanly.
- With a `sqlite:` tempfile `LATTICE_DB_URL`, appending a `#CLV1` `test` line to `<root>/.lattice/clv.ndjson`
  (the collector path) results in a `test_results` row in that SQLite file within the collector budget — an
  end-to-end test drives the real `run` + collector and queries the DB back.
- `RunHandle::shutdown` aborts the `store_task` (no leaked task) — asserted the same way the existing
  `watcher_task`/`collector_task` aborts are; a test confirms the store task is cancelled.
- **Raw stdout is never persisted:** an untagged/malformed line appended to the sink produces **no** DB
  row (only the structured `test`/`status`/`hot_edge` envelopes do) — asserted end-to-end.
- The `store_task` never panics on a DB error: a persist failure is logged and the task continues (the
  collector/ws path is unaffected by storage being slow or erroring).

### Definition of Done
- New end-to-end tests (real `run` with a tempfile sqlite DB) written and green; new-code coverage ≥ 90%.
- `just qg` clean with no external database.
- Doc cascade: `run`'s doc updated to describe the optional persistence task + `LATTICE_DB_URL`;
  `RunHandle`'s doc updated for the new task field; the app.rs module `//!` doc notes storage is
  write-through and opt-in.

---

## Dependency graph

```
P7-1 (trait + deps + scheme) ──► P7-2 (SQLite backend) ──► P7-3 (Postgres backend, gated parity)
                                        │
                                        └────────────────► P7-4 (wire into app::run)
```

- **Wave 1:** P7-1 (foundation — no deps).
- **Wave 2 (after P7-1):** P7-2 (SQLite backend + schema).
- **Wave 3 (parallel, after P7-2):** P7-3 (Postgres + parity) and P7-4 (app wiring) — disjoint files
  (`storage/postgres.rs`+`storage/mod.rs` vs `app.rs`), so they run in parallel.
- `storage/mod.rs` is touched by P7-1, P7-2, P7-3; the chain P7-1→P7-2→P7-3 serialises those edits, and
  P7-4 (app.rs only) shares no `Touches` path with P7-3. Acyclic.
- The BUILD_PLAN acceptance ("same run persists identically to SQLite and a Docker Postgres; only
  structured CLV events written") is met by P7-2 (SQLite live) + P7-3 (Postgres parity, run against a
  Docker Postgres when the daemon is up) + P7-4's end-to-end "raw stdout never persisted" assertion. The
  no-database default path (persistence opt-in via `LATTICE_DB_URL`) keeps `just qg` green with no daemon.
