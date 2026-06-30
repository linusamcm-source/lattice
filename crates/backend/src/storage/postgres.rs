//! Postgres persistence backend (`DATA_MODEL.md` §B, shared/team).
//!
//! [`PostgresStore`] is the network/team [`Storage`](super::Storage) implementation:
//! it holds a `sqlx::PgPool`, creates the §B.6 seven-table schema idempotently
//! ([`PostgresStore::ensure_schema`]), and write-throughs each structured
//! [`EventEnvelope`] to its row(s) ([`PostgresStore::persist`]). It is the **twin**
//! of [`SqliteStore`](super::sqlite::SqliteStore) — same schema, same §B.5 /
//! decision-4 persistence mapping — differing **only in SQL dialect**.
//!
//! **Dialect differences vs the SQLite backend** (everything else mirrors it):
//! - **Bind placeholders** are `$1, $2, …` (numbered) rather than SQLite's `?`.
//! - **Types** are Postgres-native: `process_id` / `last_process_id` / `duration_ms`
//!   are `BIGINT`, `hot` is a real `BOOLEAN` (not a `0`/`1` `INTEGER`), and ids /
//!   timestamps stay `TEXT` (decision-3 rfc3339, byte-identical across backends).
//! - **Upserts** use `INSERT … ON CONFLICT (id) DO UPDATE SET …` (Postgres has no
//!   `INSERT OR REPLACE`) for `nodes`/`edges`/`agents`, and `ON CONFLICT DO NOTHING`
//!   for the lazy `sessions` upsert (SQLite's `INSERT OR IGNORE`).
//! - **FK enforcement** is **native** — Postgres always enforces the §B.6
//!   `REFERENCES` constraints, so there is no per-connection `PRAGMA` to set; this is
//!   the parity target the SQLite `PRAGMA foreign_keys = ON` (decision-5) matches.
//! - **Connection** is a `PgPool` from a `postgres://…` URL. `sqlx` is built with
//!   **no TLS feature** (`default-features = false`), so connections are **plaintext
//!   only**: a local Docker Postgres with `sslmode=disable` (the plaintext default)
//!   is the target — TLS is never required.
//!
//! **§B.6 reconciliations with the real wire data (decision-4)** — identical to the
//! SQLite backend, repeated here because they are part of this backend's contract:
//! - `test_results.process_id` / `agent_activity.process_id` are **NULLable**
//!   (relaxing §B.6's `NOT NULL`): a pid-less event persists with `process_id = NULL`
//!   and **no** `agents` row (a NULL FK is unconstrained, so the
//!   `REFERENCES agents(process_id)` still holds for the `Some` case).
//! - On an `agents` upsert (only when a `pid` is present), `agent_type` defaults to
//!   the event's `agent` id (or `"unknown"`) and `color` to the placeholder
//!   `"#888888"`, satisfying the `NOT NULL` columns; the Phase-8 roster refines them.
//! - Event-log row ids (`test_results.id`) have no wire source, so they are minted as
//!   fresh **UUID v4**s; two events get two distinct ids.
//! - Event rows reference the **event's own** `session_id` (§B.2), and a `sessions`
//!   row is **lazily upserted per distinct session_id on first sight** to satisfy the
//!   `REFERENCES sessions(session_id)` FK. Per-event insert order: `sessions` →
//!   `agents` (when pid present) → the event row.
//! - `hot_edge` persists **only** as the `edges.hot` flag (§B.6 has no hot-edge
//!   table); the UPDATE is a no-op when the edge row is absent.
//! - `agent_activity` / `protocol_versions` are written by the **Phase-8 agent
//!   layer** (the SQLite twin's write paths, in Postgres dialect): an `agent.roster`
//!   ([`Payload::AgentRoster`]) upserts one **real** `agents` row per [`AgentInfo`]
//!   (its true `agent_type`/`color`/`status`, keyed by `process_id` with
//!   `ON CONFLICT (process_id)` so a re-emitted roster flips `status`/`updated_at`
//!   without duplicating — never the `#888888`/agent-id placeholders) plus one
//!   `protocol_versions` row per process; an `agent.activity`
//!   ([`Payload::AgentActivity`]) inserts one `agent_activity` row (UUID id),
//!   upserting its `agents` parent first so the `process_id` FK holds.
//!
//! Only a typed [`EventEnvelope`] reaches [`PostgresStore::persist`] — raw stdout is
//! never persisted (§B.5).
//!
//! **Read side (story P9-1).** [`PostgresStore::load_nodes`] /
//! [`PostgresStore::load_edges`] are the Postgres twins of the SQLite read methods —
//! same column reconstruction, differing only in the `$1` bind — feeding the
//! crash-rebuild warm start ([`crate::graph::Graph::from_records`]). The Docker-gated
//! `pg_load_nodes_edges_parity_with_sqlite` test exercises them **directly**: it persists
//! a parsed multi-file set through both backends, then asserts `load_nodes`/`load_edges`
//! return byte-equal reconstructed nodes (`id`/`type`/`label`/`parentId`/`status`/
//! `signature`/`meta`) and edges (`source`/`target`/`kind`) across Postgres and SQLite.
//! The separate `pg_parity_with_sqlite` harness covers the **write** side via row counts.
//! Both are skipped when `LATTICE_TEST_PG` is unset, so `just qg` is green with no daemon.
//!
//! **Live Docker-Postgres parity run (acceptance).** The parity integration test is
//! `#[ignore]`-by-default and additionally gated on the `LATTICE_TEST_PG` env var
//! (a `postgres://…` URL), so `just qg` is fully green with **no daemon**. To run it
//! against a Docker Postgres (the BUILD_PLAN "same run persists identically to SQLite
//! and a Docker Postgres" check):
//!
//! ```text
//! docker run --rm -e POSTGRES_PASSWORD=lattice -e POSTGRES_USER=lattice \
//!     -e POSTGRES_DB=lattice -p 5432:5432 postgres:16
//! LATTICE_TEST_PG='postgres://lattice:lattice@127.0.0.1:5432/lattice?sslmode=disable' \
//!     cargo test -p lattice-backend -- --ignored storage::postgres::tests::pg_parity
//! ```

use std::str::FromStr;

use async_trait::async_trait;
use serde::Serialize;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{Storage, StorageError};
use crate::wire::{AgentInfo, Edge, EventEnvelope, HotEdgeState, Node, Payload};

/// The §B.6 schema in **Postgres dialect**, as idempotent `CREATE … IF NOT EXISTS`
/// statements (decision-2).
///
/// Each entry is run once by [`PostgresStore::ensure_schema`]. The seven tables match
/// the §B.6 column sets with the Postgres-native types: `process_id` /
/// `last_process_id` / `duration_ms` are `BIGINT`, `hot` is a real `BOOLEAN`, and
/// ids/timestamps are `TEXT` (decision-3). Two **noted §B.6 relaxations**
/// (decision-4): `test_results.process_id` and `agent_activity.process_id` drop the
/// `NOT NULL` so a pid-less event persists with a NULL (unconstrained) FK. The eight
/// indexes are the §B.7 set. Postgres enforces every `REFERENCES` natively, so no
/// per-connection pragma is needed (the parity target of SQLite's
/// `PRAGMA foreign_keys = ON`).
const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS sessions (
        session_id TEXT PRIMARY KEY,
        started_at TEXT NOT NULL,
        repo_path  TEXT NOT NULL,
        label      TEXT
    )",
    "CREATE TABLE IF NOT EXISTS agents (
        process_id BIGINT PRIMARY KEY,
        agent_id   TEXT NOT NULL,
        agent_type TEXT NOT NULL,
        color      TEXT NOT NULL,
        status     TEXT NOT NULL,
        session_id TEXT NOT NULL REFERENCES sessions(session_id),
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS protocol_versions (
        process_id    BIGINT PRIMARY KEY REFERENCES agents(process_id),
        version       TEXT NOT NULL,
        session_id    TEXT NOT NULL REFERENCES sessions(session_id),
        introduced_at TEXT NOT NULL,
        deprecated_at TEXT,
        features_json TEXT
    )",
    "CREATE TABLE IF NOT EXISTS nodes (
        id              TEXT PRIMARY KEY,
        session_id      TEXT NOT NULL REFERENCES sessions(session_id),
        type            TEXT NOT NULL,
        label           TEXT NOT NULL,
        parent_id       TEXT,
        status          TEXT NOT NULL,
        docs            TEXT,
        signature_json  TEXT,
        meta_json       TEXT,
        last_process_id BIGINT,
        last_agent_id   TEXT,
        updated_at      TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS edges (
        id         TEXT PRIMARY KEY,
        session_id TEXT NOT NULL REFERENCES sessions(session_id),
        source     TEXT NOT NULL,
        target     TEXT NOT NULL,
        kind       TEXT NOT NULL,
        hot        BOOLEAN NOT NULL DEFAULT FALSE
    )",
    // process_id is NULLable (decision-4) — relaxes §B.6's NOT NULL so a pid-less
    // test.result persists with a NULL (unconstrained) FK and no agents row.
    "CREATE TABLE IF NOT EXISTS test_results (
        id          TEXT PRIMARY KEY,
        process_id  BIGINT REFERENCES agents(process_id),
        session_id  TEXT NOT NULL REFERENCES sessions(session_id),
        node_id     TEXT NOT NULL,
        test_id     TEXT NOT NULL,
        outcome     TEXT NOT NULL,
        duration_ms BIGINT,
        agent_id    TEXT,
        message     TEXT,
        ts          TEXT NOT NULL
    )",
    // process_id is NULLable (decision-4); written by the Phase-8 agent layer (P8-3).
    "CREATE TABLE IF NOT EXISTS agent_activity (
        id         TEXT PRIMARY KEY,
        process_id BIGINT REFERENCES agents(process_id),
        session_id TEXT NOT NULL REFERENCES sessions(session_id),
        agent_id   TEXT NOT NULL,
        action     TEXT NOT NULL,
        node_id    TEXT NOT NULL,
        ts         TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_nodes_parent_id ON nodes(parent_id)",
    "CREATE INDEX IF NOT EXISTS idx_nodes_session_type ON nodes(session_id, type)",
    "CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source)",
    "CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target)",
    "CREATE INDEX IF NOT EXISTS idx_edges_kind ON edges(kind)",
    "CREATE INDEX IF NOT EXISTS idx_agents_session_status ON agents(session_id, status)",
    "CREATE INDEX IF NOT EXISTS idx_test_results_node_ts ON test_results(node_id, ts)",
    "CREATE INDEX IF NOT EXISTS idx_agent_activity_node_ts ON agent_activity(node_id, ts)",
];

// --- Per-payload DML, Postgres dialect ($N placeholders + ON CONFLICT) ----------
//
// These are named consts (the SQLite backend inlines the equivalents) so the
// Postgres dialect — numbered `$N` binds and `ON CONFLICT` upserts — is asserted by
// a **hermetic, no-database** unit test (`postgres_dml_uses_numbered_placeholders`).

/// Inserts one `test_results` row (UUID id, NULLable `process_id`); 10 binds.
const SQL_INSERT_TEST_RESULT: &str = "INSERT INTO test_results
       (id, process_id, session_id, node_id, test_id, outcome, duration_ms, agent_id, message, ts)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)";

/// Updates a node's `status`/`updated_at` (`status.update`); 3 binds.
const SQL_UPDATE_NODE_STATUS: &str = "UPDATE nodes SET status = $1, updated_at = $2 WHERE id = $3";

/// Toggles an edge's `hot` flag (`hot_edge`); a no-op when the edge is absent; 2 binds.
const SQL_UPDATE_EDGE_HOT: &str = "UPDATE edges SET hot = $1 WHERE id = $2";

/// Inserts-or-updates a `nodes` row by primary key (`node.upsert`); 12 binds.
const SQL_UPSERT_NODE: &str = "INSERT INTO nodes
       (id, session_id, type, label, parent_id, status, docs, signature_json, meta_json, last_process_id, last_agent_id, updated_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
     ON CONFLICT (id) DO UPDATE SET
       session_id = excluded.session_id,
       type = excluded.type,
       label = excluded.label,
       parent_id = excluded.parent_id,
       status = excluded.status,
       docs = excluded.docs,
       signature_json = excluded.signature_json,
       meta_json = excluded.meta_json,
       last_process_id = excluded.last_process_id,
       last_agent_id = excluded.last_agent_id,
       updated_at = excluded.updated_at";

/// Inserts-or-updates an `edges` row by primary key (`edge.upsert`); 6 binds.
const SQL_UPSERT_EDGE: &str = "INSERT INTO edges (id, session_id, source, target, kind, hot)
     VALUES ($1, $2, $3, $4, $5, $6)
     ON CONFLICT (id) DO UPDATE SET
       session_id = excluded.session_id,
       source = excluded.source,
       target = excluded.target,
       kind = excluded.kind,
       hot = excluded.hot";

/// Selects a session's `nodes` rows for the P9-1 crash-rebuild read path; 1 bind. The
/// column order matches the [`Node`] reconstruction tuple in [`PostgresStore::load_nodes`].
const SQL_SELECT_NODES: &str =
    "SELECT id, type, label, parent_id, status, docs, signature_json, meta_json
     FROM nodes WHERE session_id = $1";

/// Selects a session's `edges` rows for the P9-1 crash-rebuild read path; 1 bind. The
/// column order matches the [`Edge`] reconstruction tuple in [`PostgresStore::load_edges`].
const SQL_SELECT_EDGES: &str =
    "SELECT id, source, target, kind, hot FROM edges WHERE session_id = $1";

/// Deletes a node by id (`node.remove`); 1 bind.
const SQL_DELETE_NODE: &str = "DELETE FROM nodes WHERE id = $1";

/// Deletes an edge by id (`edge.remove`); 1 bind.
const SQL_DELETE_EDGE: &str = "DELETE FROM edges WHERE id = $1";

/// Lazily upserts a `sessions` row, ignoring a duplicate `session_id`; 3 binds.
/// Shared by [`upsert_session`] and [`PostgresStore::record_session`].
const SQL_UPSERT_SESSION: &str = "INSERT INTO sessions (session_id, started_at, repo_path)
     VALUES ($1, $2, $3)
     ON CONFLICT (session_id) DO NOTHING";

/// Upserts an `agents` row, refreshing `status`/`updated_at` on conflict; 7 binds
/// (`status` is the literal `'active'`).
const SQL_UPSERT_AGENT: &str = "INSERT INTO agents
       (process_id, agent_id, agent_type, color, status, session_id, created_at, updated_at)
     VALUES ($1, $2, $3, $4, 'active', $5, $6, $7)
     ON CONFLICT (process_id) DO UPDATE SET status = 'active', updated_at = excluded.updated_at";

/// Upserts a **real** `agents` row from a roster `AgentInfo` (true
/// `agent_id`/`agent_type`/`color`/`status`, not the placeholders), refreshing the
/// identity/metadata, `session_id`, and `updated_at` on conflict (`agent.roster`); 8
/// binds. The conflict clause refreshes `session_id` so a reused PID re-homes to its
/// new session, matching [`SQL_UPSERT_PROTOCOL_VERSION`].
const SQL_UPSERT_ROSTER_AGENT: &str = "INSERT INTO agents
       (process_id, agent_id, agent_type, color, status, session_id, created_at, updated_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
     ON CONFLICT (process_id) DO UPDATE SET
       agent_id = excluded.agent_id,
       agent_type = excluded.agent_type,
       color = excluded.color,
       status = excluded.status,
       session_id = excluded.session_id,
       updated_at = excluded.updated_at";

/// Upserts one `protocol_versions` row per process, refreshing on conflict
/// (`agent.roster`); 4 binds.
const SQL_UPSERT_PROTOCOL_VERSION: &str = "INSERT INTO protocol_versions
       (process_id, version, session_id, introduced_at)
     VALUES ($1, $2, $3, $4)
     ON CONFLICT (process_id) DO UPDATE SET
       version = excluded.version,
       session_id = excluded.session_id,
       introduced_at = excluded.introduced_at";

/// Inserts one `agent_activity` row (UUID id, NULLable `process_id`)
/// (`agent.activity`); 7 binds.
const SQL_INSERT_AGENT_ACTIVITY: &str = "INSERT INTO agent_activity
       (id, process_id, session_id, agent_id, action, node_id, ts)
     VALUES ($1, $2, $3, $4, $5, $6, $7)";

/// Upper bound on opening a connection (the pool's `acquire_timeout`).
///
/// 5s is long enough not to be flaky against a slow-but-live Postgres, yet far below
/// `sqlx`'s 30s default — so a dead/unreachable host (the hermetic connect-failure
/// tests, and a misconfigured `LATTICE_DB_URL` at startup once wired into `app::run`)
/// errors in ~5s rather than ~30s. A timeout is still a [`StorageError::Db`].
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The Postgres [`Storage`](super::Storage) backend (`DATA_MODEL.md` §B, team).
///
/// Wraps a `sqlx::PgPool`. Construct it with [`PostgresStore::connect`] (the
/// [`open_store`](super::open_store) factory's Postgres arm); call
/// [`PostgresStore::ensure_schema`] once before persisting. Postgres enforces the
/// §B.6 foreign keys natively (no pragma), the parity target of the SQLite backend's
/// `PRAGMA foreign_keys = ON`. All queries use `sqlx`'s runtime API (no `query!`
/// macro), so no `DATABASE_URL` is needed to build or test.
pub struct PostgresStore {
    /// Connection pool to the shared Postgres instance.
    pool: PgPool,
}

impl PostgresStore {
    /// Opens a [`PostgresStore`] from a `postgres://…` URL.
    ///
    /// The URL is parsed by `sqlx`'s [`PgConnectOptions`]; the pool eagerly
    /// establishes and tests one connection, so an unreachable host or a malformed
    /// URL returns a [`StorageError::Db`] rather than panicking. Because `sqlx` is
    /// built with **no TLS feature**, the connection is **plaintext only** — point at
    /// a Postgres that accepts plaintext (`sslmode=disable`, the default for a local
    /// Docker server). Does **not** create the schema — call
    /// [`PostgresStore::ensure_schema`] after opening.
    ///
    /// The eager connect is bounded by [`CONNECT_TIMEOUT`] (5s) via the pool's
    /// `acquire_timeout`, so a dead or misconfigured host **fails fast** instead of
    /// blocking on `sqlx`'s 30s default — keeping `just qg` quick and avoiding a long
    /// startup hang once this is wired into `app::run`. The error semantics are
    /// unchanged: a timeout still surfaces as a [`StorageError::Db`], never a panic.
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let options = PgConnectOptions::from_str(url)?;
        let pool = PgPoolOptions::new()
            .acquire_timeout(CONNECT_TIMEOUT)
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl Storage for PostgresStore {
    /// Creates the §B.6 seven-table schema and §B.7 indexes idempotently by running
    /// each [`SCHEMA`] `CREATE … IF NOT EXISTS` statement (Postgres dialect); running
    /// it twice is a no-op. Called once after [`PostgresStore::connect`].
    async fn ensure_schema(&self) -> Result<(), StorageError> {
        for statement in SCHEMA {
            sqlx::query(statement).execute(&self.pool).await?;
        }
        Ok(())
    }

    /// Write-throughs one structured [`EventEnvelope`] per the §B.5 / decision-4
    /// rules (see the module doc), with **all of an event's writes in one
    /// transaction** — identical semantics to the SQLite backend, in Postgres
    /// dialect (`$N` binds, `ON CONFLICT` upserts).
    ///
    /// Each call opens a single transaction so the event's `sessions`/`agents`/event
    /// rows commit atomically. The per-event timestamp is computed once and reused by
    /// every write. Dispatches on the [`Payload`]: `test.result` inserts a
    /// `test_results` row (UUID id, NULL `process_id` when the event has no pid);
    /// `status.update` updates `nodes.status`; `hot_edge` updates `edges.hot` (a
    /// no-op when the edge is absent); `node.upsert`/`edge.upsert` insert-or-update;
    /// `node.remove`/`edge.remove` delete; `agent.roster` upserts a **real** `agents`
    /// row per [`AgentInfo`] plus a `protocol_versions` row per process;
    /// `agent.activity` inserts an `agent_activity` row (upserting its `agents` parent
    /// first for the FK); `snapshot`/`subtree` (view frames) persist nothing. A
    /// `sessions` row is upserted per event (the DB no-ops a duplicate), and an
    /// `agents` row when a pid is present, before the event row.
    async fn persist(&self, env: &EventEnvelope) -> Result<(), StorageError> {
        // One timestamp per event, reused by every write below (decision-3).
        let now = now_rfc3339();
        // One transaction per event: atomic writes.
        let mut tx = self.pool.begin().await?;
        match &env.payload {
            Payload::TestResult {
                node_id,
                test_id,
                outcome,
                duration_ms,
                session_id,
                agent_id,
                process_id,
                message,
            } => {
                upsert_session(&mut tx, session_id, &now).await?;
                if let Some(pid) = *process_id {
                    upsert_agent(&mut tx, pid, agent_id.as_deref(), session_id, &now).await?;
                }
                sqlx::query(SQL_INSERT_TEST_RESULT)
                    .bind(Uuid::new_v4().to_string())
                    .bind(process_id.map(i64::from))
                    .bind(session_id.as_str())
                    .bind(node_id.as_str())
                    .bind(test_id.as_str())
                    .bind(wire_enum_str(outcome))
                    .bind(duration_ms.map(|d| i64::try_from(d).unwrap_or(i64::MAX)))
                    .bind(agent_id.as_deref())
                    .bind(message.as_deref())
                    .bind(now.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            Payload::StatusUpdate {
                node_id,
                status,
                session_id,
                agent_id,
                process_id,
            } => {
                upsert_session(&mut tx, session_id, &now).await?;
                if let Some(pid) = *process_id {
                    upsert_agent(&mut tx, pid, agent_id.as_deref(), session_id, &now).await?;
                }
                sqlx::query(SQL_UPDATE_NODE_STATUS)
                    .bind(wire_enum_str(status))
                    .bind(now.as_str())
                    .bind(node_id.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            Payload::HotEdge {
                edge_id,
                state,
                process_id,
                session_id,
                agent_id,
                ts: _,
            } => {
                upsert_session(&mut tx, session_id, &now).await?;
                if let Some(pid) = *process_id {
                    upsert_agent(&mut tx, pid, agent_id.as_deref(), session_id, &now).await?;
                }
                sqlx::query(SQL_UPDATE_EDGE_HOT)
                    .bind(matches!(state, HotEdgeState::Enter))
                    .bind(edge_id.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            Payload::NodeUpsert { node } => {
                upsert_session(&mut tx, &env.session_id, &now).await?;
                let signature_json = node
                    .signature
                    .as_ref()
                    .and_then(|s| serde_json::to_string(s).ok());
                let meta_json = node
                    .meta
                    .as_ref()
                    .and_then(|m| serde_json::to_string(m).ok());
                sqlx::query(SQL_UPSERT_NODE)
                    .bind(node.id.as_str())
                    .bind(env.session_id.as_str())
                    .bind(wire_enum_str(&node.node_type))
                    .bind(node.label.as_str())
                    .bind(node.parent_id.as_deref())
                    .bind(wire_enum_str(&node.status))
                    .bind(node.docs.as_deref())
                    .bind(signature_json.as_deref())
                    .bind(meta_json.as_deref())
                    .bind(None::<i64>)
                    .bind(None::<&str>)
                    .bind(now.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            Payload::EdgeUpsert { edge } => {
                upsert_session(&mut tx, &env.session_id, &now).await?;
                sqlx::query(SQL_UPSERT_EDGE)
                    .bind(edge.id.as_str())
                    .bind(env.session_id.as_str())
                    .bind(edge.source.as_str())
                    .bind(edge.target.as_str())
                    .bind(wire_enum_str(&edge.kind))
                    .bind(edge.hot)
                    .execute(&mut *tx)
                    .await?;
            }
            Payload::NodeRemove { id } => {
                sqlx::query(SQL_DELETE_NODE)
                    .bind(id.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            Payload::EdgeRemove { id } => {
                sqlx::query(SQL_DELETE_EDGE)
                    .bind(id.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            Payload::Snapshot { .. } | Payload::Subtree { .. } => {
                // View frames (server→client) — persist nothing (§B.5).
            }
            Payload::AgentActivity {
                agent_id,
                action,
                node_id,
                session_id,
                process_id,
                ts: _,
                msg: _,
            } => {
                upsert_session(&mut tx, session_id, &now).await?;
                // Upsert the agents parent first (placeholder metadata — a later
                // roster refines it) so the process_id FK holds with no prior roster.
                if let Some(pid) = *process_id {
                    upsert_agent(&mut tx, pid, Some(agent_id.as_str()), session_id, &now).await?;
                }
                sqlx::query(SQL_INSERT_AGENT_ACTIVITY)
                    .bind(Uuid::new_v4().to_string())
                    .bind(process_id.map(i64::from))
                    .bind(session_id.as_str())
                    .bind(agent_id.as_str())
                    .bind(action.as_str())
                    .bind(node_id.as_str())
                    .bind(now.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            Payload::AgentRoster { session_id, agents } => {
                upsert_session(&mut tx, session_id, &now).await?;
                for info in agents {
                    upsert_roster_agent(&mut tx, info, session_id, &now).await?;
                    upsert_protocol_version(&mut tx, info, session_id, &now).await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Upserts the run's `sessions` row (`session_id`, `started_at`, `repo_path`)
    /// with `ON CONFLICT (session_id) DO NOTHING`, so re-recording the same run is a
    /// no-op.
    async fn record_session(&self, session_id: &str, repo_path: &str) -> Result<(), StorageError> {
        sqlx::query(SQL_UPSERT_SESSION)
            .bind(session_id)
            .bind(now_rfc3339())
            .bind(repo_path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Loads every `nodes` row for `session_id` — the Postgres twin of
    /// [`SqliteStore::load_nodes`](super::sqlite::SqliteStore::load_nodes), differing
    /// only in the `$1` bind. Reconstructs each [`Node`] from the columns the
    /// `node.upsert` write arm bound; `type`/`status` parse back via
    /// [`wire_enum_from_str`] and `signature`/`meta` via [`parse_json_column`].
    /// **`child_ids` is unpersisted** and loads empty (re-derived by
    /// [`crate::graph::Graph::from_records`], Design Decision #7). Never panics on
    /// malformed stored data.
    async fn load_nodes(&self, session_id: &str) -> Result<Vec<Node>, StorageError> {
        let rows: Vec<(
            String,         // id
            String,         // type
            String,         // label
            Option<String>, // parent_id
            String,         // status
            Option<String>, // docs
            Option<String>, // signature_json
            Option<String>, // meta_json
        )> = sqlx::query_as(SQL_SELECT_NODES)
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?;

        let mut nodes = Vec::with_capacity(rows.len());
        for (id, node_type, label, parent_id, status, docs, signature_json, meta_json) in rows {
            nodes.push(Node {
                id,
                node_type: wire_enum_from_str(&node_type)?,
                label,
                parent_id,
                child_ids: Vec::new(),
                status: wire_enum_from_str(&status)?,
                docs,
                signature: parse_json_column(signature_json.as_deref())?,
                meta: parse_json_column(meta_json.as_deref())?,
            });
        }
        Ok(nodes)
    }

    /// Loads every `edges` row for `session_id` — the Postgres twin of
    /// [`SqliteStore::load_edges`](super::sqlite::SqliteStore::load_edges). `kind`
    /// parses back via [`wire_enum_from_str`] and `hot` decodes from the `BOOLEAN`
    /// column. Never panics on malformed stored data.
    async fn load_edges(&self, session_id: &str) -> Result<Vec<Edge>, StorageError> {
        let rows: Vec<(String, String, String, String, bool)> = sqlx::query_as(SQL_SELECT_EDGES)
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?;

        let mut edges = Vec::with_capacity(rows.len());
        for (id, source, target, kind, hot) in rows {
            edges.push(Edge {
                id,
                source,
                target,
                kind: wire_enum_from_str(&kind)?,
                hot,
            });
        }
        Ok(edges)
    }
}

/// Serialises a small CLV enum (e.g. `NodeType`, `NodeStatus`, `TestOutcome`,
/// `EdgeKind`) to its canonical wire string for a TEXT column.
///
/// Goes through `serde_json` so the persisted value stays in lock-step with the
/// `wire.rs` serde mapping (and byte-identical with the SQLite backend). Falls back
/// to an empty string if the value does not serialise to a JSON string (unreachable
/// for these unit enums), never panicking. A local twin of the SQLite backend's
/// helper (kept per-module so this story does not touch `sqlite.rs`).
fn wire_enum_str<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

/// Parses a stored canonical wire string back into its CLV enum — the inverse of
/// [`wire_enum_str`] used by the P9-1 load path. A local twin of the SQLite backend's
/// helper (kept per-module so this story does not couple the two backends). An
/// unrecognised value yields a [`StorageError::Config`] rather than panicking.
fn wire_enum_from_str<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|err| StorageError::Config(format!("unrecognised wire enum value '{s}': {err}")))
}

/// Deserialises an optional JSON TEXT column (`signature_json` / `meta_json`) back to
/// its typed value for the P9-1 load path: `Ok(None)` for a NULL column and a
/// [`StorageError::Config`] (never a panic) for malformed JSON. A local twin of the
/// SQLite backend's helper.
fn parse_json_column<T: serde::de::DeserializeOwned>(
    raw: Option<&str>,
) -> Result<Option<T>, StorageError> {
    match raw {
        Some(text) => serde_json::from_str(text)
            .map(Some)
            .map_err(|err| StorageError::Config(format!("corrupt JSON column '{text}': {err}"))),
        None => Ok(None),
    }
}

/// Ensures a `sessions` row for `session_id` exists, enlisting in the caller's
/// transaction via `conn` (decision-4).
///
/// Runs `ON CONFLICT (session_id) DO NOTHING` on **every** event (idempotent — the DB
/// no-ops a duplicate, so no in-memory dedup set is needed). An already-recorded run
/// session (from [`PostgresStore::record_session`], carrying the real `repo_path`) is
/// preserved; an event-discovered session has no known `repo_path`, so an empty
/// string satisfies the `NOT NULL` column. Satisfies the event tables'
/// `REFERENCES sessions(session_id)`. `now` is the per-event timestamp the caller
/// computed once.
async fn upsert_session(
    conn: &mut PgConnection,
    session_id: &str,
    now: &str,
) -> Result<(), StorageError> {
    sqlx::query(SQL_UPSERT_SESSION)
        .bind(session_id)
        .bind(now)
        .bind("")
        .execute(conn)
        .await?;
    Ok(())
}

/// Upserts an `agents` row for `process_id`, defaulting the metadata the wire omits,
/// enlisting in the caller's transaction via `conn` (decision-4).
///
/// Called only when an event carries a `pid`. `agent_id`/`agent_type` default to the
/// event's `agent` id (or `"unknown"` when absent), `color` to the placeholder
/// `"#888888"`, and `status` to `"active"` (§B.3). On a repeat `process_id` the row's
/// `status`/`updated_at` are refreshed. The caller must have upserted the `sessions`
/// row first (FK `agents.session_id`). `now` is the per-event timestamp the caller
/// computed once.
async fn upsert_agent(
    conn: &mut PgConnection,
    process_id: u32,
    agent_id: Option<&str>,
    session_id: &str,
    now: &str,
) -> Result<(), StorageError> {
    let agent = agent_id.unwrap_or("unknown");
    sqlx::query(SQL_UPSERT_AGENT)
        .bind(i64::from(process_id))
        .bind(agent)
        .bind(agent)
        .bind("#888888")
        .bind(session_id)
        .bind(now)
        .bind(now)
        .execute(conn)
        .await?;
    Ok(())
}

/// Upserts a **real** `agents` row from an [`AgentInfo`] roster entry, enlisting in the
/// caller's transaction via `conn` (P8-3) — the Postgres twin of the SQLite backend's
/// `upsert_roster_agent`.
///
/// Unlike the bare-pid [`upsert_agent`] (which stamps the `#888888`/agent-id
/// placeholders), this writes the roster's true `agent_id`/`agent_type`/`color`/
/// `status`, keyed by `process_id`. `ON CONFLICT (process_id) DO UPDATE` refreshes the
/// identity/metadata, `session_id`, and `updated_at` on a re-emitted roster (e.g. a
/// `status` flip), so a process keeps exactly one row — and refines any placeholder row
/// a prior pid-bearing event seeded. Refreshing `session_id` keeps a reused PID
/// consistent with its `protocol_versions` row across sessions. The caller must have
/// upserted the `sessions` row first (FK `agents.session_id`). `now` is the per-event
/// timestamp the caller computed once.
async fn upsert_roster_agent(
    conn: &mut PgConnection,
    info: &AgentInfo,
    session_id: &str,
    now: &str,
) -> Result<(), StorageError> {
    sqlx::query(SQL_UPSERT_ROSTER_AGENT)
        .bind(i64::from(info.process_id))
        .bind(info.agent_id.as_str())
        .bind(info.agent_type.as_str())
        .bind(info.color.as_str())
        .bind(info.status.as_str())
        .bind(session_id)
        .bind(now)
        .bind(now)
        .execute(conn)
        .await?;
    Ok(())
}

/// Upserts a `protocol_versions` row for an [`AgentInfo`]'s process, enlisting in the
/// caller's transaction via `conn` (P8-3) — the Postgres twin of the SQLite backend's
/// `upsert_protocol_version`.
///
/// `version` is the roster's `protocol_version`, defaulting to `"1"` when absent;
/// `introduced_at` is the per-event `now`. `ON CONFLICT (process_id) DO UPDATE`
/// refreshes the row on a re-emitted roster, so a process keeps exactly one row. The
/// caller must have upserted the `sessions` row **and** this process's `agents` row
/// (via [`upsert_roster_agent`]) first, satisfying the `REFERENCES sessions` and
/// `REFERENCES agents(process_id)` FKs.
async fn upsert_protocol_version(
    conn: &mut PgConnection,
    info: &AgentInfo,
    session_id: &str,
    now: &str,
) -> Result<(), StorageError> {
    let version = info.protocol_version.as_deref().unwrap_or("1");
    sqlx::query(SQL_UPSERT_PROTOCOL_VERSION)
        .bind(i64::from(info.process_id))
        .bind(version)
        .bind(session_id)
        .bind(now)
        .execute(conn)
        .await?;
    Ok(())
}

/// Returns a best-effort RFC3339 UTC timestamp for a TEXT timestamp column
/// (decision-3).
///
/// A chrono-free copy of the SQLite backend's `now_rfc3339` (kept per-module so this
/// story does not touch `sqlite.rs`/`graph.rs`): panic-free total-integer arithmetic
/// via Howard Hinnant's `civil_from_days`. Stores a stable, byte-identical TEXT form
/// across both backends.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (hour, minute, second) = (tod / 3_600, (tod % 3_600) / 60, tod % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every per-payload DML string, the shared session/agent upserts, and the P8-3
    /// agent-layer writers (roster agent, protocol version, activity insert).
    const ALL_DML: [&str; 12] = [
        SQL_INSERT_TEST_RESULT,
        SQL_UPDATE_NODE_STATUS,
        SQL_UPDATE_EDGE_HOT,
        SQL_UPSERT_NODE,
        SQL_UPSERT_EDGE,
        SQL_DELETE_NODE,
        SQL_DELETE_EDGE,
        SQL_UPSERT_SESSION,
        SQL_UPSERT_AGENT,
        SQL_UPSERT_ROSTER_AGENT,
        SQL_UPSERT_PROTOCOL_VERSION,
        SQL_INSERT_AGENT_ACTIVITY,
    ];

    #[test]
    fn postgres_schema_uses_postgres_dialect_types() {
        // Hermetic (no DB): the Postgres schema uses BIGINT/BOOLEAN, never the
        // SQLite INTEGER/0-1 forms, and applies the decision-4 NULLable relaxation.
        let ddl = SCHEMA.join("\n");
        assert!(
            ddl.contains("process_id BIGINT"),
            "process_id must be BIGINT"
        );
        assert!(
            ddl.contains("hot        BOOLEAN NOT NULL DEFAULT FALSE"),
            "hot must be a real BOOLEAN"
        );
        assert!(
            ddl.contains("duration_ms BIGINT"),
            "duration_ms must be BIGINT"
        );
        assert!(
            !ddl.contains("INTEGER"),
            "no SQLite INTEGER types in the Postgres schema"
        );
        // decision-4: the two event tables relax process_id to NULLable.
        assert!(
            ddl.contains("process_id  BIGINT REFERENCES agents(process_id)"),
            "test_results/agent_activity process_id must be NULLable BIGINT"
        );
    }

    #[test]
    fn postgres_dml_uses_numbered_placeholders() {
        // Hermetic (no DB): every DML string uses Postgres `$N` binds, never the
        // SQLite `?` placeholder.
        for sql in ALL_DML {
            assert!(
                !sql.contains('?'),
                "Postgres DML must not use a `?` placeholder: {sql}"
            );
            assert!(
                sql.contains("$1"),
                "Postgres DML must use numbered `$N` placeholders: {sql}"
            );
        }
        // The upserts must be Postgres ON CONFLICT, not SQLite INSERT OR REPLACE/IGNORE.
        assert!(SQL_UPSERT_NODE.contains("ON CONFLICT (id) DO UPDATE SET"));
        assert!(SQL_UPSERT_EDGE.contains("ON CONFLICT (id) DO UPDATE SET"));
        assert!(SQL_UPSERT_AGENT.contains("ON CONFLICT (process_id) DO UPDATE SET"));
        assert!(SQL_UPSERT_SESSION.contains("ON CONFLICT (session_id) DO NOTHING"));
        // The P8-3 agent-layer upserts are keyed on process_id, too.
        assert!(SQL_UPSERT_ROSTER_AGENT.contains("ON CONFLICT (process_id) DO UPDATE SET"));
        assert!(SQL_UPSERT_PROTOCOL_VERSION.contains("ON CONFLICT (process_id) DO UPDATE SET"));
        for sql in ALL_DML {
            assert!(
                !sql.contains("INSERT OR"),
                "Postgres DML must not use a SQLite `INSERT OR …`: {sql}"
            );
        }
    }

    #[test]
    fn copied_helpers_match_sqlite_behaviour() {
        // Hermetic (no DB): the byte-for-byte copies of the SQLite backend's helpers
        // behave identically. `wire_enum_str` must yield the canonical wire strings the
        // TEXT columns store — the same forms `persist` binds for `outcome`, `kind`,
        // `status`, and `type` — and `now_rfc3339` a parseable RFC3339 UTC instant.
        assert_eq!(wire_enum_str(&TestOutcome::Pass), "pass");
        assert_eq!(wire_enum_str(&TestOutcome::Fail), "fail");
        assert_eq!(wire_enum_str(&EdgeKind::Calls), "calls");
        assert_eq!(wire_enum_str(&EdgeKind::DataFlowsFrom), "data_flows_from");
        assert_eq!(wire_enum_str(&NodeStatus::Failing), "failing");
        assert_eq!(wire_enum_str(&NodeType::Function), "function");

        let ts = now_rfc3339();
        // YYYY-MM-DDThh:mm:ssZ — 20 chars, 4-digit numeric year, 'T' separator, 'Z'.
        assert_eq!(ts.len(), 20, "unexpected rfc3339 length: {ts}");
        assert!(ts.ends_with('Z'), "rfc3339 must end with Z: {ts}");
        assert!(ts.contains('T'), "rfc3339 must have a T separator: {ts}");
        let year = ts.get(0..4).unwrap_or_default();
        assert!(
            year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()),
            "rfc3339 must start with a 4-digit year: {ts}"
        );
    }

    #[tokio::test]
    async fn connect_to_unreachable_host_returns_storage_error() {
        // Hermetic (no Postgres): connecting to a refused port must return a
        // StorageError::Db, never panic. 127.0.0.1:1 is refused immediately (no DNS).
        let res = PostgresStore::connect("postgres://lattice:lattice@127.0.0.1:1/lattice").await;
        match res {
            Err(StorageError::Db(_)) => {}
            Err(other) => panic!("expected a Db error, got {other:?}"),
            Ok(_) => panic!("unexpectedly connected to an unreachable Postgres"),
        }
    }

    // --- Gated live Docker-Postgres parity test --------------------------------
    //
    // `#[ignore]` AND env-gated on `LATTICE_TEST_PG`: it is SKIPPED by `cargo test`
    // (not `--ignored`) and, even with `--ignored`, no-ops with a printed skip notice
    // when `LATTICE_TEST_PG` is unset — so `just qg` is green with no daemon. See the
    // module doc for the Docker run procedure.

    use crate::wire::{
        AgentInfo, Edge, EdgeKind, EventType, Node, NodeStatus, NodeType, Signature, TestOutcome,
    };

    const TABLES: [&str; 7] = [
        "sessions",
        "agents",
        "protocol_versions",
        "nodes",
        "edges",
        "test_results",
        "agent_activity",
    ];

    fn test_result_env(session: &str, pid: Option<u32>) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: session.to_string(),
            event_type: EventType::TestResult,
            payload: Payload::TestResult {
                node_id: "fn:a.rs:f".to_string(),
                test_id: "auth::case".to_string(),
                outcome: TestOutcome::Pass,
                duration_ms: Some(7),
                session_id: session.to_string(),
                agent_id: Some("tdd-green".to_string()),
                process_id: pid,
                message: Some("ok".to_string()),
            },
        }
    }

    fn node_upsert_env() -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: "sess-1".to_string(),
            event_type: EventType::NodeUpsert,
            payload: Payload::NodeUpsert {
                node: Node {
                    id: "fn:a.rs:f".to_string(),
                    node_type: NodeType::Function,
                    label: "f".to_string(),
                    parent_id: Some("file:a.rs".to_string()),
                    child_ids: Vec::new(),
                    status: NodeStatus::Unknown,
                    docs: Some("docs".to_string()),
                    signature: Some(Signature {
                        params: Vec::new(),
                        returns: "()".to_string(),
                    }),
                    meta: None,
                },
            },
        }
    }

    fn edge_upsert_env() -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: "sess-1".to_string(),
            event_type: EventType::EdgeUpsert,
            payload: Payload::EdgeUpsert {
                edge: Edge {
                    id: "e:a->b".to_string(),
                    source: "fn:a.rs:a".to_string(),
                    target: "fn:a.rs:b".to_string(),
                    kind: EdgeKind::Calls,
                    hot: false,
                },
            },
        }
    }

    fn hot_edge_env() -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: "sess-1".to_string(),
            event_type: EventType::HotEdge,
            payload: Payload::HotEdge {
                edge_id: "e:a->b".to_string(),
                state: HotEdgeState::Enter,
                process_id: None,
                session_id: "sess-1".to_string(),
                agent_id: None,
                ts: "2026-06-30T00:00:00Z".to_string(),
            },
        }
    }

    /// The full structured envelope sequence both backends persist for the parity
    /// comparison (a pid-bearing + a pid-less test result, a node/edge upsert, a
    /// hot-edge toggle).
    fn parity_envelopes() -> Vec<EventEnvelope> {
        vec![
            test_result_env("sess-1", Some(48213)),
            test_result_env("sess-1", None),
            node_upsert_env(),
            edge_upsert_env(),
            hot_edge_env(),
        ]
    }

    /// Builds one [`AgentInfo`] roster row with explicit metadata (P8-3 fixtures).
    fn agent_info(
        pid: u32,
        agent_id: &str,
        agent_type: &str,
        color: &str,
        status: &str,
        protocol_version: Option<&str>,
    ) -> AgentInfo {
        AgentInfo {
            process_id: pid,
            agent_id: agent_id.to_string(),
            agent_type: agent_type.to_string(),
            color: color.to_string(),
            status: status.to_string(),
            protocol_version: protocol_version.map(str::to_string),
        }
    }

    /// Wraps a set of [`AgentInfo`] rows in an `agent.roster` envelope (P8-3 fixtures).
    fn agent_roster_env(session: &str, agents: Vec<AgentInfo>) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: session.to_string(),
            event_type: EventType::AgentRoster,
            payload: Payload::AgentRoster {
                session_id: session.to_string(),
                agents,
            },
        }
    }

    /// Builds an `agent.activity` envelope for `agent_id`/`action`/`node_id` (P8-3).
    fn agent_activity_env(
        session: &str,
        agent_id: &str,
        action: &str,
        node_id: &str,
        pid: Option<u32>,
    ) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: session.to_string(),
            event_type: EventType::AgentActivity,
            payload: Payload::AgentActivity {
                agent_id: agent_id.to_string(),
                action: action.to_string(),
                node_id: node_id.to_string(),
                session_id: session.to_string(),
                process_id: pid,
                ts: None,
                msg: None,
            },
        }
    }

    /// Wraps `node` in a `node.upsert` envelope under `session` (P9-1 read-parity).
    fn node_upsert_for(session: &str, node: Node) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: session.to_string(),
            event_type: EventType::NodeUpsert,
            payload: Payload::NodeUpsert { node },
        }
    }

    /// Wraps `edge` in an `edge.upsert` envelope under `session` (P9-1 read-parity).
    fn edge_upsert_for(session: &str, edge: Edge) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: session.to_string(),
            event_type: EventType::EdgeUpsert,
            payload: Payload::EdgeUpsert { edge },
        }
    }

    async fn pg_count(pool: &PgPool, table: &str) -> i64 {
        // `table` is a fixed test constant, never user input.
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .expect("pg count")
    }

    async fn sqlite_count(pool: &sqlx::SqlitePool, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .expect("sqlite count")
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via LATTICE_TEST_PG (Docker); run with --ignored"]
    async fn pg_parity_with_sqlite() {
        use super::super::sqlite::SqliteStore;
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;

        let pg_url = match std::env::var("LATTICE_TEST_PG") {
            Ok(u) if !u.trim().is_empty() => u,
            _ => {
                eprintln!("skipped: LATTICE_TEST_PG unset — Postgres parity test not run");
                return;
            }
        };

        // --- Postgres side: open, reset to a clean schema, persist the sequence. ---
        let pg = PostgresStore::connect(&pg_url)
            .await
            .expect("connect Postgres");
        // Isolate the run: drop the seven tables (FK order handled by CASCADE) then
        // recreate them, so repeated runs are deterministic.
        sqlx::query(
            "DROP TABLE IF EXISTS test_results, agent_activity, protocol_versions, nodes, edges, agents, sessions CASCADE",
        )
        .execute(&pg.pool)
        .await
        .expect("reset Postgres schema");
        pg.ensure_schema().await.expect("pg ensure_schema");

        // --- SQLite side: a fresh tempfile DB with the same schema. ---
        let dir = tempfile::tempdir().expect("tempdir");
        let sqlite_url = format!("sqlite://{}", dir.path().join("parity.db").display());
        let sqlite = SqliteStore::connect(&sqlite_url)
            .await
            .expect("connect SQLite");
        sqlite.ensure_schema().await.expect("sqlite ensure_schema");

        // Persist the identical sequence through both backends.
        for env in parity_envelopes() {
            pg.persist(&env).await.expect("pg persist");
            sqlite.persist(&env).await.expect("sqlite persist");
        }

        // A second read pool over the same SQLite file, FK enforcement ON (parity).
        let sqlite_ro = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::from_str(&sqlite_url)
                    .expect("sqlite options")
                    .foreign_keys(true),
            )
            .await
            .expect("sqlite read pool");

        // Parity: every table's row count matches across the two backends.
        for table in TABLES {
            let p = pg_count(&pg.pool, table).await;
            let s = sqlite_count(&sqlite_ro, table).await;
            assert_eq!(p, s, "row-count parity mismatch for table {table}");
        }

        // The pid-bearing test result created exactly one agent under both; the
        // pid-less one created none — so agents == 1 across both.
        assert_eq!(pg_count(&pg.pool, "agents").await, 1, "pg agents count");
        assert_eq!(
            sqlite_count(&sqlite_ro, "agents").await,
            1,
            "sqlite agents count"
        );
        assert_eq!(
            pg_count(&pg.pool, "test_results").await,
            2,
            "pg test_results count"
        );

        // Pid-less NULL parity: exactly one test_results row has a NULL process_id.
        let pg_null: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM test_results WHERE process_id IS NULL")
                .fetch_one(&pg.pool)
                .await
                .expect("pg null count");
        let sqlite_null: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM test_results WHERE process_id IS NULL")
                .fetch_one(&sqlite_ro)
                .await
                .expect("sqlite null count");
        assert_eq!(pg_null, 1, "pg pid-less NULL process_id");
        assert_eq!(sqlite_null, 1, "sqlite pid-less NULL process_id");

        // agent_activity / protocol_versions stay empty in Phase 7 under both.
        for table in ["agent_activity", "protocol_versions"] {
            assert_eq!(pg_count(&pg.pool, table).await, 0, "pg {table} non-empty");
            assert_eq!(
                sqlite_count(&sqlite_ro, table).await,
                0,
                "sqlite {table} non-empty"
            );
        }

        // FK-violating insert rejected under BOTH backends (Postgres natively,
        // SQLite via PRAGMA foreign_keys = ON). Seed a valid session so the failing
        // FK is purely the missing agents(process_id) parent.
        sqlx::query("INSERT INTO sessions (session_id, started_at, repo_path) VALUES ($1, $2, $3)")
            .bind("fk-sess")
            .bind("t")
            .bind("/r")
            .execute(&pg.pool)
            .await
            .expect("pg seed session");
        let pg_fk = sqlx::query(
            "INSERT INTO test_results (id, process_id, session_id, node_id, test_id, outcome, ts)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind("x")
        .bind(999_999_i64)
        .bind("fk-sess")
        .bind("n")
        .bind("t")
        .bind("pass")
        .bind("ts")
        .execute(&pg.pool)
        .await;
        assert!(pg_fk.is_err(), "Postgres must reject the orphan FK row");

        sqlx::query("INSERT INTO sessions (session_id, started_at, repo_path) VALUES (?, ?, ?)")
            .bind("fk-sess")
            .bind("t")
            .bind("/r")
            .execute(&sqlite_ro)
            .await
            .expect("sqlite seed session");
        let sqlite_fk = sqlx::query(
            "INSERT INTO test_results (id, process_id, session_id, node_id, test_id, outcome, ts)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("x")
        .bind(999_999_i64)
        .bind("fk-sess")
        .bind("n")
        .bind("t")
        .bind("pass")
        .bind("ts")
        .execute(&sqlite_ro)
        .await;
        assert!(sqlite_fk.is_err(), "SQLite must reject the orphan FK row");
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via LATTICE_TEST_PG (Docker); run with --ignored"]
    async fn pg_agent_layer_parity_with_sqlite() {
        // P8-3 / AC6: the agent-layer round-trip — an `agent.roster` (two agents) plus an
        // `agent.activity` — persists IDENTICALLY across Postgres and SQLite. Gated like
        // `pg_parity_with_sqlite`: SKIPPED by `cargo test` (`#[ignore]`) and a no-op when
        // `LATTICE_TEST_PG` is unset, so `just qg` is green with no Docker daemon. RED
        // (when run with --ignored against a live PG) until P8-3 implements the writers.
        //
        // The roster lists pid 48590 as `inactive`, then the activity is emitted for that
        // SAME pid: the activity arm's `upsert_agent` does `ON CONFLICT (process_id) DO
        // UPDATE SET status = 'active'`, so an agent that emits activity becomes active
        // (correct, §B.3) — the final assert checks 48590 flips to `active`, identically
        // on both backends.
        use super::super::sqlite::SqliteStore;
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;

        let pg_url = match std::env::var("LATTICE_TEST_PG") {
            Ok(u) if !u.trim().is_empty() => u,
            _ => {
                eprintln!("skipped: LATTICE_TEST_PG unset — Postgres agent-layer parity not run");
                return;
            }
        };

        // --- Postgres side: open, reset to a clean schema, persist the sequence. ---
        let pg = PostgresStore::connect(&pg_url)
            .await
            .expect("connect Postgres");
        sqlx::query(
            "DROP TABLE IF EXISTS test_results, agent_activity, protocol_versions, nodes, edges, agents, sessions CASCADE",
        )
        .execute(&pg.pool)
        .await
        .expect("reset Postgres schema");
        pg.ensure_schema().await.expect("pg ensure_schema");

        // --- SQLite side: a fresh tempfile DB with the same schema. ---
        let dir = tempfile::tempdir().expect("tempdir");
        let sqlite_url = format!("sqlite://{}", dir.path().join("agent_parity.db").display());
        let sqlite = SqliteStore::connect(&sqlite_url)
            .await
            .expect("connect SQLite");
        sqlite.ensure_schema().await.expect("sqlite ensure_schema");

        let envelopes = vec![
            agent_roster_env(
                "sess-1",
                vec![
                    agent_info(
                        48213,
                        "tdd-green",
                        "implementation",
                        "#2ecc71",
                        "active",
                        Some("1"),
                    ),
                    agent_info(
                        48590,
                        "security-scanner",
                        "security",
                        "#e67e22",
                        "inactive",
                        Some("1"),
                    ),
                ],
            ),
            agent_activity_env(
                "sess-1",
                "security-scanner",
                "modified",
                "fn:a.rs:f",
                Some(48590),
            ),
        ];
        for env in &envelopes {
            pg.persist(env).await.expect("pg persist");
            sqlite.persist(env).await.expect("sqlite persist");
        }

        // A second read pool over the same SQLite file, FK enforcement ON (parity).
        let sqlite_ro = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::from_str(&sqlite_url)
                    .expect("sqlite options")
                    .foreign_keys(true),
            )
            .await
            .expect("sqlite read pool");

        // Row-count parity across the three agent-layer tables.
        for table in ["agents", "protocol_versions", "agent_activity"] {
            let p = pg_count(&pg.pool, table).await;
            let s = sqlite_count(&sqlite_ro, table).await;
            assert_eq!(p, s, "agent-layer row-count parity mismatch for {table}");
        }
        assert_eq!(pg_count(&pg.pool, "agents").await, 2, "two roster agents");
        assert_eq!(
            pg_count(&pg.pool, "agent_activity").await,
            1,
            "one agent_activity row"
        );
        assert_eq!(
            pg_count(&pg.pool, "protocol_versions").await,
            2,
            "one protocol_versions row per process"
        );

        // Real roster metadata (not the bare-pid placeholders) under Postgres.
        let color: String = sqlx::query_scalar("SELECT color FROM agents WHERE process_id = $1")
            .bind(48213_i64)
            .fetch_one(&pg.pool)
            .await
            .expect("pg agent color");
        assert_eq!(color, "#2ecc71", "roster color must be the real metadata");
        // pid 48590 was rostered `inactive`, then emitted an `agent.activity`, whose
        // `upsert_agent` flips it to `active` (`ON CONFLICT … SET status = 'active'`).
        // Assert that correct flip — and that it is IDENTICAL across both backends.
        let pg_status: String =
            sqlx::query_scalar("SELECT status FROM agents WHERE process_id = $1")
                .bind(48590_i64)
                .fetch_one(&pg.pool)
                .await
                .expect("pg agent status");
        assert_eq!(
            pg_status, "active",
            "an agent that emits activity becomes active"
        );
        let sqlite_status: String =
            sqlx::query_scalar("SELECT status FROM agents WHERE process_id = ?")
                .bind(48590_i64)
                .fetch_one(&sqlite_ro)
                .await
                .expect("sqlite agent status");
        assert_eq!(
            pg_status, sqlite_status,
            "agent status after activity must match across both backends"
        );
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via LATTICE_TEST_PG (Docker); run with --ignored"]
    async fn pg_load_nodes_edges_parity_with_sqlite() {
        // Finding #1 (P9-1 read path): load_nodes/load_edges must return identical
        // reconstructed nodes/edges across Postgres and SQLite for the SAME persisted
        // multi-file source set. Gated exactly like the write-side parity test: SKIPPED
        // by `cargo test` (`#[ignore]`) and a no-op when `LATTICE_TEST_PG` is unset, so
        // `just qg` is green with no Docker daemon.
        use super::super::sqlite::SqliteStore;
        use crate::parser::parse_source;

        let pg_url = match std::env::var("LATTICE_TEST_PG") {
            Ok(u) if !u.trim().is_empty() => u,
            _ => {
                eprintln!("skipped: LATTICE_TEST_PG unset — Postgres load parity not run");
                return;
            }
        };

        // --- Postgres side: open, reset to a clean schema. ---
        let pg = PostgresStore::connect(&pg_url)
            .await
            .expect("connect Postgres");
        sqlx::query(
            "DROP TABLE IF EXISTS test_results, agent_activity, protocol_versions, nodes, edges, agents, sessions CASCADE",
        )
        .execute(&pg.pool)
        .await
        .expect("reset Postgres schema");
        pg.ensure_schema().await.expect("pg ensure_schema");

        // --- SQLite side: a fresh tempfile DB with the same schema. ---
        let dir = tempfile::tempdir().expect("tempdir");
        let sqlite_url = format!("sqlite://{}", dir.path().join("load_parity.db").display());
        let sqlite = SqliteStore::connect(&sqlite_url)
            .await
            .expect("connect SQLite");
        sqlite.ensure_schema().await.expect("sqlite ensure_schema");

        // Persist a parsed multi-file set (functions, variables, contains + derived
        // edges, signature/meta) through BOTH backends under one session.
        let sources = [
            (
                "a.rs",
                "fn alpha() { let x = 1; let y = beta(); }\nfn beta() -> i32 { 0 }",
            ),
            ("b.rs", "fn gamma() {}"),
        ];
        for (path, src) in sources {
            let parsed = parse_source(path, src);
            for node in parsed.nodes {
                let env = node_upsert_for("sess-1", node);
                pg.persist(&env).await.expect("pg node persist");
                sqlite.persist(&env).await.expect("sqlite node persist");
            }
            for edge in parsed.edges {
                let env = edge_upsert_for("sess-1", edge);
                pg.persist(&env).await.expect("pg edge persist");
                sqlite.persist(&env).await.expect("sqlite edge persist");
            }
        }

        // --- Node read parity (load order is unspecified — sort by id first). ---
        let mut pg_nodes = pg.load_nodes("sess-1").await.expect("pg load_nodes");
        let mut sqlite_nodes = sqlite
            .load_nodes("sess-1")
            .await
            .expect("sqlite load_nodes");
        pg_nodes.sort_by(|a, b| a.id.cmp(&b.id));
        sqlite_nodes.sort_by(|a, b| a.id.cmp(&b.id));

        assert!(
            !pg_nodes.is_empty(),
            "the fixture must persist at least one node"
        );
        assert_eq!(
            pg_nodes.len(),
            sqlite_nodes.len(),
            "node count must match across backends"
        );
        for (p, s) in pg_nodes.iter().zip(&sqlite_nodes) {
            assert_eq!(p.id, s.id, "node id parity");
            assert_eq!(p.node_type, s.node_type, "node type parity for {}", p.id);
            assert_eq!(p.label, s.label, "node label parity for {}", p.id);
            assert_eq!(
                p.parent_id, s.parent_id,
                "node parentId parity for {}",
                p.id
            );
            assert_eq!(p.status, s.status, "node status parity for {}", p.id);
            assert_eq!(
                p.signature, s.signature,
                "node signature parity for {}",
                p.id
            );
            assert_eq!(p.meta, s.meta, "node meta parity for {}", p.id);
            assert!(
                p.child_ids.is_empty(),
                "child_ids is unpersisted and must load empty for {}",
                p.id
            );
        }

        // --- Edge read parity. ---
        let mut pg_edges = pg.load_edges("sess-1").await.expect("pg load_edges");
        let mut sqlite_edges = sqlite
            .load_edges("sess-1")
            .await
            .expect("sqlite load_edges");
        pg_edges.sort_by(|a, b| a.id.cmp(&b.id));
        sqlite_edges.sort_by(|a, b| a.id.cmp(&b.id));

        assert!(
            !pg_edges.is_empty(),
            "the fixture must persist at least one edge"
        );
        assert_eq!(
            pg_edges.len(),
            sqlite_edges.len(),
            "edge count must match across backends"
        );
        for (p, s) in pg_edges.iter().zip(&sqlite_edges) {
            assert_eq!(p.id, s.id, "edge id parity");
            assert_eq!(p.source, s.source, "edge source parity for {}", p.id);
            assert_eq!(p.target, s.target, "edge target parity for {}", p.id);
            assert_eq!(p.kind, s.kind, "edge kind parity for {}", p.id);
        }
    }
}
