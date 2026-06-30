//! SQLite persistence backend (`DATA_MODEL.md` §B, solo/local).
//!
//! [`SqliteStore`] is the file-based [`Storage`](super::Storage) implementation: it
//! holds a `sqlx::SqlitePool`, creates the §B.6 seven-table schema idempotently
//! ([`SqliteStore::ensure_schema`]), and write-throughs each structured
//! [`EventEnvelope`] to its row(s) ([`SqliteStore::persist`]). It is the live half
//! of the dual-backend design — the Postgres twin (P7-3) shares this schema and
//! mapping, differing only in dialect (`$N` placeholders, `BIGINT`/`BOOLEAN`).
//!
//! **FK parity (decision-5).** The pool opens every connection with
//! `PRAGMA foreign_keys = ON` (via [`sqlx::sqlite::SqliteConnectOptions::foreign_keys`])
//! so SQLite enforces the §B.6 `REFERENCES` constraints identically to Postgres;
//! without it an FK-violating row would insert silently under SQLite.
//!
//! **§B.6 reconciliations with the real wire data (decision-4).** The §B.6 DDL
//! assumes a `pid` and full agent metadata on every event, but the Phase-5/6 wire
//! payloads frequently omit them, so this backend applies these *noted* §B.6
//! extensions:
//! - `test_results.process_id` / `agent_activity.process_id` are **NULLable**
//!   (relaxing §B.6's `NOT NULL`): a pid-less event persists with `process_id =
//!   NULL` and **no** `agents` row (a NULL FK is unconstrained, so the
//!   `REFERENCES agents(process_id)` still holds for the `Some` case).
//! - On an `agents` upsert (only when a `pid` is present), `agent_type` defaults to
//!   the event's `agent` id (or `"unknown"`) and `color` to the placeholder
//!   `"#888888"`, satisfying the `NOT NULL` columns; the Phase-8 roster refines them.
//! - Event-log row ids (`test_results.id`) have no wire source, so they are minted
//!   as fresh **UUID v4**s; two events get two distinct ids.
//! - Event rows reference the **event's own** `session_id` (the payload CLV session,
//!   §B.2), and a `sessions` row is **lazily upserted per distinct session_id on
//!   first sight** to satisfy the `REFERENCES sessions(session_id)` FK. Per-event
//!   insert order: `sessions` → `agents` (when pid present) → the event row.
//! - `hot_edge` persists **only** as the `edges.hot` flag (§B.6 has no hot-edge
//!   table); the UPDATE is a no-op when the edge row is absent.
//! - `agent_activity` / `protocol_versions` are written by the **Phase-8 agent
//!   layer**: an `agent.roster` ([`Payload::AgentRoster`]) upserts one **real**
//!   `agents` row per [`AgentInfo`] (its true `agent_type`/`color`/`status`, keyed by
//!   `process_id` with `ON CONFLICT(process_id)` so a re-emitted roster flips
//!   `status`/`updated_at` without duplicating the row — never the `#888888`/agent-id
//!   placeholders) plus one `protocol_versions` row per process; an `agent.activity`
//!   ([`Payload::AgentActivity`]) inserts one `agent_activity` row (UUID id), upserting
//!   its `agents` parent first so the `process_id` `REFERENCES agents(process_id)` FK
//!   holds even with no prior roster.
//! - Timestamps are stored as **TEXT rfc3339** (decision-3): a chrono-free format
//!   that stores byte-identically under both backends.
//!
//! Only a typed [`EventEnvelope`] (the collector's parsed output) reaches
//! [`SqliteStore::persist`] — raw stdout is never persisted (§B.5).
//!
//! **Read side (story P9-1).** [`SqliteStore::load_nodes`] / [`SqliteStore::load_edges`]
//! reconstruct a session's persisted [`Node`](crate::wire::Node)s /
//! [`Edge`](crate::wire::Edge)s for the crash-rebuild warm start, parsing the wire
//! enums back with [`wire_enum_from_str`] (the inverse of [`wire_enum_str`]) and the
//! JSON columns with [`parse_json_column`]. `child_ids` is unpersisted and loads empty
//! (re-derived by [`crate::graph::Graph::from_records`], Design Decision #7).

use std::str::FromStr;

use async_trait::async_trait;
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{Storage, StorageError};
use crate::wire::{AgentInfo, Edge, EventEnvelope, HotEdgeState, Node, Payload};

/// The §B.6 schema in SQLite dialect, as idempotent `CREATE … IF NOT EXISTS`
/// statements (decision-2).
///
/// Each entry is run once by [`SqliteStore::ensure_schema`]. The seven tables match
/// the §B.6 column sets (TEXT for ids/timestamps, INTEGER for the `BIGINT`
/// `process_id`, INTEGER `0`/`1` for the `BOOLEAN` `hot`), with two **noted §B.6
/// relaxations** (decision-4): `test_results.process_id` and
/// `agent_activity.process_id` drop the `NOT NULL` so a pid-less event can persist
/// with a NULL (unconstrained) FK. The eight indexes are the §B.7 set.
const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS sessions (
        session_id TEXT PRIMARY KEY,
        started_at TEXT NOT NULL,
        repo_path  TEXT NOT NULL,
        label      TEXT
    )",
    "CREATE TABLE IF NOT EXISTS agents (
        process_id INTEGER PRIMARY KEY,
        agent_id   TEXT NOT NULL,
        agent_type TEXT NOT NULL,
        color      TEXT NOT NULL,
        status     TEXT NOT NULL,
        session_id TEXT NOT NULL REFERENCES sessions(session_id),
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS protocol_versions (
        process_id    INTEGER PRIMARY KEY REFERENCES agents(process_id),
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
        last_process_id INTEGER,
        last_agent_id   TEXT,
        updated_at      TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS edges (
        id         TEXT PRIMARY KEY,
        session_id TEXT NOT NULL REFERENCES sessions(session_id),
        source     TEXT NOT NULL,
        target     TEXT NOT NULL,
        kind       TEXT NOT NULL,
        hot        INTEGER NOT NULL DEFAULT 0
    )",
    // process_id is NULLable (decision-4) — relaxes §B.6's NOT NULL so a pid-less
    // test.result persists with a NULL (unconstrained) FK and no agents row.
    "CREATE TABLE IF NOT EXISTS test_results (
        id          TEXT PRIMARY KEY,
        process_id  INTEGER REFERENCES agents(process_id),
        session_id  TEXT NOT NULL REFERENCES sessions(session_id),
        node_id     TEXT NOT NULL,
        test_id     TEXT NOT NULL,
        outcome     TEXT NOT NULL,
        duration_ms INTEGER,
        agent_id    TEXT,
        message     TEXT,
        ts          TEXT NOT NULL
    )",
    // process_id is NULLable (decision-4); written by the Phase-8 agent layer (P8-3).
    "CREATE TABLE IF NOT EXISTS agent_activity (
        id         TEXT PRIMARY KEY,
        process_id INTEGER REFERENCES agents(process_id),
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

/// The SQLite [`Storage`](super::Storage) backend (`DATA_MODEL.md` §B, solo/local).
///
/// Wraps a `sqlx::SqlitePool` opened with `PRAGMA foreign_keys = ON` (decision-5).
/// Construct it with [`SqliteStore::connect`] (the [`open_store`](super::open_store)
/// factory's SQLite arm); call [`SqliteStore::ensure_schema`] once before persisting.
/// All queries use `sqlx`'s runtime API (no `query!` macro), so no `DATABASE_URL` is
/// needed to build or test.
pub struct SqliteStore {
    /// Connection pool; every connection has foreign-key enforcement enabled.
    pool: SqlitePool,
}

impl SqliteStore {
    /// Opens a [`SqliteStore`] from a `sqlite:` URL, creating the database file if
    /// missing, enabling `PRAGMA foreign_keys = ON` on every connection (decision-5),
    /// and running in **WAL** journal mode with **NORMAL** synchronous for lower
    /// per-event write latency (WAL appends sequentially; NORMAL fsyncs only at
    /// checkpoint — durable enough here, as the in-memory `Graph` is the source of
    /// truth and crash-replay is Phase 9). WAL creates `-wal`/`-shm` sidecar files
    /// beside the database file, which is expected.
    ///
    /// The URL is parsed by `sqlx`'s [`sqlx::sqlite::SqliteConnectOptions`], which
    /// accepts the `sqlite:`, `sqlite://`, `sqlite:///abs`, and `sqlite::memory:`
    /// forms. Does **not** create the schema — call [`SqliteStore::ensure_schema`]
    /// after opening. Never panics: a malformed URL or connect failure returns a
    /// [`StorageError::Db`].
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl Storage for SqliteStore {
    /// Creates the §B.6 seven-table schema and §B.7 indexes idempotently by running
    /// each [`SCHEMA`] `CREATE … IF NOT EXISTS` statement; running it twice is a
    /// no-op. Called once after [`SqliteStore::connect`].
    async fn ensure_schema(&self) -> Result<(), StorageError> {
        for statement in SCHEMA {
            sqlx::query(statement).execute(&self.pool).await?;
        }
        Ok(())
    }

    /// Write-throughs one structured [`EventEnvelope`] per the §B.5 / decision-4
    /// rules (see the module doc), with **all of an event's writes in one
    /// transaction**.
    ///
    /// Each call opens a single transaction so the event's `sessions`/`agents`/event
    /// rows commit atomically — no partial-write window if a later statement fails —
    /// and share one fsync (with WAL + NORMAL synchronous). The per-event timestamp
    /// is computed once and reused by every write. Dispatches on the [`Payload`]:
    /// `test.result` inserts a `test_results` row (UUID id, NULL `process_id` when
    /// the event has no pid); `status.update` updates `nodes.status`; `hot_edge`
    /// updates `edges.hot` (a no-op when the edge is absent); `node.upsert`/
    /// `edge.upsert` insert-or-replace; `node.remove`/`edge.remove` delete;
    /// `agent.roster` upserts a **real** `agents` row per [`AgentInfo`] plus a
    /// `protocol_versions` row per process; `agent.activity` inserts an
    /// `agent_activity` row (upserting its `agents` parent first for the FK);
    /// `snapshot`/`subtree` (view frames) persist nothing. A `sessions` row is
    /// upserted per event (the DB no-ops a duplicate), and an `agents` row when a
    /// pid is present, before the event row.
    async fn persist(&self, env: &EventEnvelope) -> Result<(), StorageError> {
        // One timestamp per event, reused by every write below (decision-3).
        let now = now_rfc3339();
        // One transaction per event: atomic writes + a single fsync.
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
                sqlx::query(
                    "INSERT INTO test_results
                       (id, process_id, session_id, node_id, test_id, outcome, duration_ms, agent_id, message, ts)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
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
                sqlx::query("UPDATE nodes SET status = ?, updated_at = ? WHERE id = ?")
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
                sqlx::query("UPDATE edges SET hot = ? WHERE id = ?")
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
                sqlx::query(
                    "INSERT OR REPLACE INTO nodes
                       (id, session_id, type, label, parent_id, status, docs, signature_json, meta_json, last_process_id, last_agent_id, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
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
                sqlx::query(
                    "INSERT OR REPLACE INTO edges (id, session_id, source, target, kind, hot)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
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
                sqlx::query("DELETE FROM nodes WHERE id = ?")
                    .bind(id.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            Payload::EdgeRemove { id } => {
                sqlx::query("DELETE FROM edges WHERE id = ?")
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
                sqlx::query(
                    "INSERT INTO agent_activity
                       (id, process_id, session_id, agent_id, action, node_id, ts)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
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
    /// with `INSERT OR IGNORE`, so re-recording the same run is a no-op.
    async fn record_session(&self, session_id: &str, repo_path: &str) -> Result<(), StorageError> {
        sqlx::query(
            "INSERT OR IGNORE INTO sessions (session_id, started_at, repo_path) VALUES (?, ?, ?)",
        )
        .bind(session_id)
        .bind(now_rfc3339())
        .bind(repo_path)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Loads every `nodes` row for `session_id`, reconstructing each [`Node`] from the
    /// columns the `node.upsert` write arm bound (`id`, `type`, `label`, `parent_id`,
    /// `status`, `docs`, `signature_json`, `meta_json`).
    ///
    /// `type`/`status` are parsed back from their canonical wire strings via
    /// [`wire_enum_from_str`] (the inverse of [`wire_enum_str`]) and `signature`/`meta`
    /// from their JSON columns via [`parse_json_column`]. **`child_ids` is unpersisted**
    /// (no column), so each node loads with an empty `child_ids` — re-derived by
    /// [`crate::graph::Graph::from_records`] (Design Decision #7). Corrupt stored data
    /// surfaces a [`StorageError`] rather than panicking.
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
        )> = sqlx::query_as(
            "SELECT id, type, label, parent_id, status, docs, signature_json, meta_json
             FROM nodes WHERE session_id = ?",
        )
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

    /// Loads every `edges` row for `session_id`, reconstructing each [`Edge`]
    /// (`id`/`source`/`target`/`kind`/`hot`); `kind` is parsed from its canonical wire
    /// string via [`wire_enum_from_str`] and `hot` decodes from the `INTEGER` flag.
    /// Corrupt stored data surfaces a [`StorageError`] rather than panicking.
    async fn load_edges(&self, session_id: &str) -> Result<Vec<Edge>, StorageError> {
        let rows: Vec<(String, String, String, String, bool)> =
            sqlx::query_as("SELECT id, source, target, kind, hot FROM edges WHERE session_id = ?")
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
/// `wire.rs` serde mapping. Falls back to an empty string if the value does not
/// serialise to a JSON string (unreachable for these unit enums), never panicking.
fn wire_enum_str<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

/// Parses a stored canonical wire string back into its CLV enum (e.g. [`NodeType`],
/// [`NodeStatus`], [`EdgeKind`]) — the inverse of [`wire_enum_str`] used by the P9-1
/// load path.
///
/// Wraps the TEXT value in a `serde_json::Value::String` and deserialises through the
/// same `serde` mapping the write side used, so the round-trip stays in lock-step with
/// `wire.rs`. An unrecognised value yields a [`StorageError::Config`] rather than
/// panicking (the never-panic-on-bad-input contract; unreachable for well-formed data).
fn wire_enum_from_str<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|err| StorageError::Config(format!("unrecognised wire enum value '{s}': {err}")))
}

/// Deserialises an optional JSON TEXT column (`signature_json` / `meta_json`) back to
/// its typed value for the P9-1 load path: `Ok(None)` for a NULL column, the decoded
/// value for valid JSON, and a [`StorageError::Config`] (never a panic) for malformed
/// JSON.
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
/// Runs `INSERT OR IGNORE` on **every** event (idempotent — the DB no-ops a
/// duplicate `session_id`, so no in-memory dedup set is needed; the per-event
/// transaction makes the repeated statement cheap). An already-recorded run session
/// (from [`SqliteStore::record_session`], carrying the real `repo_path`) is
/// preserved; an event-discovered session has no known `repo_path`, so an empty
/// string satisfies the `NOT NULL` column. Satisfies the event tables'
/// `REFERENCES sessions(session_id)`. `now` is the per-event timestamp the caller
/// computed once.
async fn upsert_session(
    conn: &mut SqliteConnection,
    session_id: &str,
    now: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT OR IGNORE INTO sessions (session_id, started_at, repo_path) VALUES (?, ?, ?)",
    )
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
    conn: &mut SqliteConnection,
    process_id: u32,
    agent_id: Option<&str>,
    session_id: &str,
    now: &str,
) -> Result<(), StorageError> {
    let agent = agent_id.unwrap_or("unknown");
    sqlx::query(
        "INSERT INTO agents (process_id, agent_id, agent_type, color, status, session_id, created_at, updated_at)
         VALUES (?, ?, ?, ?, 'active', ?, ?, ?)
         ON CONFLICT(process_id) DO UPDATE SET status = 'active', updated_at = excluded.updated_at",
    )
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

/// Upserts a **real** `agents` row from an [`AgentInfo`] roster entry, enlisting in
/// the caller's transaction via `conn` (P8-3).
///
/// Unlike the bare-pid [`upsert_agent`] (which stamps the `#888888`/agent-id
/// placeholders), this writes the roster's true `agent_id`/`agent_type`/`color`/
/// `status`, keyed by `process_id`. `ON CONFLICT(process_id) DO UPDATE` refreshes the
/// identity/metadata, `session_id`, and `updated_at` on a re-emitted roster (e.g. a
/// `status` flip), so a process keeps exactly one row — and refines any placeholder
/// row a prior pid-bearing event seeded. Refreshing `session_id` keeps a reused PID
/// consistent with its `protocol_versions` row across sessions. The caller must have
/// upserted the `sessions` row first (FK `agents.session_id`). `now` is the per-event
/// timestamp the caller computed once.
async fn upsert_roster_agent(
    conn: &mut SqliteConnection,
    info: &AgentInfo,
    session_id: &str,
    now: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO agents (process_id, agent_id, agent_type, color, status, session_id, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(process_id) DO UPDATE SET
           agent_id = excluded.agent_id,
           agent_type = excluded.agent_type,
           color = excluded.color,
           status = excluded.status,
           session_id = excluded.session_id,
           updated_at = excluded.updated_at",
    )
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
/// caller's transaction via `conn` (P8-3).
///
/// `version` is the roster's `protocol_version`, defaulting to `"1"` when absent;
/// `introduced_at` is the per-event `now`. `ON CONFLICT(process_id) DO UPDATE`
/// refreshes the row on a re-emitted roster, so a process keeps exactly one row. The
/// caller must have upserted the `sessions` row **and** this process's `agents` row
/// (via [`upsert_roster_agent`]) first, satisfying the `REFERENCES sessions` and
/// `REFERENCES agents(process_id)` FKs.
async fn upsert_protocol_version(
    conn: &mut SqliteConnection,
    info: &AgentInfo,
    session_id: &str,
    now: &str,
) -> Result<(), StorageError> {
    let version = info.protocol_version.as_deref().unwrap_or("1");
    sqlx::query(
        "INSERT INTO protocol_versions (process_id, version, session_id, introduced_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(process_id) DO UPDATE SET
           version = excluded.version,
           session_id = excluded.session_id,
           introduced_at = excluded.introduced_at",
    )
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
/// A chrono-free copy of `graph::rfc3339_now` (kept local so this story does not
/// touch `graph.rs`): panic-free total-integer arithmetic via Howard Hinnant's
/// `civil_from_days`. Phase 7 does not assert on the exact value; it only needs a
/// stable, byte-identical TEXT form across backends.
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
    use crate::parser::{parse_source, ParsedFile};
    use crate::wire::{
        AgentInfo, Edge, EdgeKind, EventType, Node, NodeStatus, NodeType, Signature, TestOutcome,
    };
    use sqlx::Row;

    /// Opens a live SQLite store backed by a tempfile (so multiple pool connections
    /// share state — unlike a per-connection `:memory:` db) with the schema applied.
    async fn temp_store() -> (SqliteStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}", dir.path().join("lattice.db").display());
        let store = SqliteStore::connect(&url).await.expect("connect");
        store.ensure_schema().await.expect("ensure_schema");
        (store, dir)
    }

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

    fn node_upsert_env(label: &str, status: NodeStatus) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: "sess-1".to_string(),
            event_type: EventType::NodeUpsert,
            payload: Payload::NodeUpsert {
                node: Node {
                    id: "fn:a.rs:f".to_string(),
                    node_type: NodeType::Function,
                    label: label.to_string(),
                    parent_id: Some("file:a.rs".to_string()),
                    child_ids: Vec::new(),
                    status,
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

    fn edge_upsert_env(hot: bool) -> EventEnvelope {
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
                    hot,
                },
            },
        }
    }

    fn hot_edge_env(edge_id: &str, state: HotEdgeState) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: "sess-1".to_string(),
            event_type: EventType::HotEdge,
            payload: Payload::HotEdge {
                edge_id: edge_id.to_string(),
                state,
                process_id: None,
                session_id: "sess-1".to_string(),
                agent_id: None,
                ts: "2026-06-30T00:00:00Z".to_string(),
            },
        }
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

    async fn count(store: &SqliteStore, table: &str) -> i64 {
        // `table` is a fixed test constant, never user input.
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&store.pool)
            .await
            .expect("count")
    }

    #[tokio::test]
    async fn connect_enables_wal_journal_mode() {
        // Perf (decision): the pool runs in WAL journal mode. WAL needs a file-based
        // DB, which `temp_store` provides; querying PRAGMA confirms the mode is live.
        let (store, _dir) = temp_store().await;
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&store.pool)
            .await
            .expect("journal_mode");
        assert_eq!(mode, "wal");
    }

    #[tokio::test]
    async fn ensure_schema_creates_all_tables_and_indexes_idempotently() {
        let (store, _dir) = temp_store().await;
        for table in TABLES {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
            )
            .bind(table)
            .fetch_one(&store.pool)
            .await
            .expect("query table");
            assert_eq!(n, 1, "missing table {table}");
        }
        let indexes = [
            "idx_nodes_parent_id",
            "idx_nodes_session_type",
            "idx_edges_source",
            "idx_edges_target",
            "idx_edges_kind",
            "idx_agents_session_status",
            "idx_test_results_node_ts",
            "idx_agent_activity_node_ts",
        ];
        for ix in indexes {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?",
            )
            .bind(ix)
            .fetch_one(&store.pool)
            .await
            .expect("query index");
            assert_eq!(n, 1, "missing index {ix}");
        }
        // Idempotent: a second call must not error.
        store
            .ensure_schema()
            .await
            .expect("second ensure_schema is a no-op");
    }

    #[tokio::test]
    async fn foreign_keys_on_rejects_orphan_test_result() {
        let (store, _dir) = temp_store().await;
        // Isolate the FK to `agents` by giving the row a valid session.
        sqlx::query(
            "INSERT INTO sessions (session_id, started_at, repo_path) VALUES ('s', 't', '/r')",
        )
        .execute(&store.pool)
        .await
        .expect("seed session");
        // process_id 999999 has no agents parent → FK must reject (PRAGMA foreign_keys=ON).
        let res = sqlx::query(
            "INSERT INTO test_results (id, process_id, session_id, node_id, test_id, outcome, ts)
             VALUES ('x', 999999, 's', 'n', 't', 'pass', 'ts')",
        )
        .execute(&store.pool)
        .await;
        assert!(res.is_err(), "FK-violating insert should be rejected");
    }

    #[tokio::test]
    async fn test_result_with_pid_round_trips_and_creates_agent() {
        let (store, _dir) = temp_store().await;
        store
            .persist(&test_result_env("sess-1", Some(48213)))
            .await
            .expect("persist");

        let row = sqlx::query(
            "SELECT id, process_id, session_id, node_id, test_id, outcome, duration_ms, agent_id, message FROM test_results",
        )
        .fetch_one(&store.pool)
        .await
        .expect("test_results row");
        let id: String = row.get("id");
        assert!(Uuid::parse_str(&id).is_ok(), "id is not a UUID: {id}");
        let pid: i64 = row.get("process_id");
        assert_eq!(pid, 48213);
        let session_id: String = row.get("session_id");
        assert_eq!(session_id, "sess-1");
        let node_id: String = row.get("node_id");
        assert_eq!(node_id, "fn:a.rs:f");
        let test_id: String = row.get("test_id");
        assert_eq!(test_id, "auth::case");
        let outcome: String = row.get("outcome");
        assert_eq!(outcome, "pass");
        let duration: i64 = row.get("duration_ms");
        assert_eq!(duration, 7);
        let agent_id: String = row.get("agent_id");
        assert_eq!(agent_id, "tdd-green");
        let message: String = row.get("message");
        assert_eq!(message, "ok");

        let agent =
            sqlx::query("SELECT agent_type, color, status FROM agents WHERE process_id = ?")
                .bind(48213_i64)
                .fetch_one(&store.pool)
                .await
                .expect("agents row");
        let agent_type: String = agent.get("agent_type");
        assert_eq!(agent_type, "tdd-green");
        let color: String = agent.get("color");
        assert_eq!(color, "#888888");
        let status: String = agent.get("status");
        assert_eq!(status, "active");
    }

    #[tokio::test]
    async fn test_result_without_pid_persists_null_and_no_agent() {
        let (store, _dir) = temp_store().await;
        store
            .persist(&test_result_env("sess-1", None))
            .await
            .expect("persist");

        let row = sqlx::query("SELECT process_id FROM test_results")
            .fetch_one(&store.pool)
            .await
            .expect("test_results row");
        let pid: Option<i64> = row.get("process_id");
        assert_eq!(pid, None, "pid-less event must store NULL process_id");
        assert_eq!(
            count(&store, "agents").await,
            0,
            "no agents row for pid-less event"
        );
        assert_eq!(count(&store, "test_results").await, 1);
    }

    #[tokio::test]
    async fn distinct_sessions_upserted_once_and_ids_distinct() {
        let (store, _dir) = temp_store().await;
        store
            .persist(&test_result_env("sess-1", None))
            .await
            .unwrap();
        store
            .persist(&test_result_env("sess-1", None))
            .await
            .unwrap();
        assert_eq!(
            count(&store, "sessions").await,
            1,
            "same session upserted once"
        );

        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM test_results ORDER BY id")
            .fetch_all(&store.pool)
            .await
            .expect("ids");
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "two events must get distinct UUIDs");

        store
            .persist(&test_result_env("sess-2", None))
            .await
            .unwrap();
        assert_eq!(
            count(&store, "sessions").await,
            2,
            "distinct session adds a row"
        );
    }

    #[tokio::test]
    async fn status_update_sets_node_status() {
        let (store, _dir) = temp_store().await;
        store
            .persist(&node_upsert_env("f", NodeStatus::Unknown))
            .await
            .unwrap();
        let su = EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: "sess-1".to_string(),
            event_type: EventType::StatusUpdate,
            payload: Payload::StatusUpdate {
                node_id: "fn:a.rs:f".to_string(),
                status: NodeStatus::Failing,
                session_id: "sess-1".to_string(),
                agent_id: None,
                process_id: None,
            },
        };
        store.persist(&su).await.unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM nodes WHERE id = 'fn:a.rs:f'")
            .fetch_one(&store.pool)
            .await
            .expect("status");
        assert_eq!(status, "failing");
    }

    #[tokio::test]
    async fn hot_edge_updates_edge_and_is_noop_when_absent() {
        let (store, _dir) = temp_store().await;
        // No-op when the edge row is absent: must not error or create a row.
        store
            .persist(&hot_edge_env("e:a->b", HotEdgeState::Enter))
            .await
            .expect("hot_edge no-op");
        assert_eq!(
            count(&store, "edges").await,
            0,
            "hot_edge must not create an edge"
        );

        store.persist(&edge_upsert_env(false)).await.unwrap();
        store
            .persist(&hot_edge_env("e:a->b", HotEdgeState::Enter))
            .await
            .unwrap();
        let hot: bool = sqlx::query_scalar("SELECT hot FROM edges WHERE id = 'e:a->b'")
            .fetch_one(&store.pool)
            .await
            .expect("hot");
        assert!(hot, "edge should be hot after enter");

        store
            .persist(&hot_edge_env("e:a->b", HotEdgeState::Exit))
            .await
            .unwrap();
        let hot: bool = sqlx::query_scalar("SELECT hot FROM edges WHERE id = 'e:a->b'")
            .fetch_one(&store.pool)
            .await
            .expect("hot");
        assert!(!hot, "edge should be cold after exit");
    }

    #[tokio::test]
    async fn node_and_edge_upsert_replace_and_remove() {
        let (store, _dir) = temp_store().await;
        store
            .persist(&node_upsert_env("f", NodeStatus::Unknown))
            .await
            .unwrap();
        store
            .persist(&node_upsert_env("g", NodeStatus::Passing))
            .await
            .unwrap();
        assert_eq!(
            count(&store, "nodes").await,
            1,
            "upsert replaces, not duplicates"
        );
        let row = sqlx::query("SELECT label, status, type FROM nodes WHERE id = 'fn:a.rs:f'")
            .fetch_one(&store.pool)
            .await
            .expect("node row");
        let label: String = row.get("label");
        assert_eq!(label, "g");
        let status: String = row.get("status");
        assert_eq!(status, "passing");
        let node_type: String = row.get("type");
        assert_eq!(node_type, "function");

        store.persist(&edge_upsert_env(false)).await.unwrap();
        store.persist(&edge_upsert_env(true)).await.unwrap();
        assert_eq!(count(&store, "edges").await, 1);

        let remove_node = EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: "sess-1".to_string(),
            event_type: EventType::NodeRemove,
            payload: Payload::NodeRemove {
                id: "fn:a.rs:f".to_string(),
            },
        };
        store.persist(&remove_node).await.unwrap();
        assert_eq!(count(&store, "nodes").await, 0);

        let remove_edge = EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: "sess-1".to_string(),
            event_type: EventType::EdgeRemove,
            payload: Payload::EdgeRemove {
                id: "e:a->b".to_string(),
            },
        };
        store.persist(&remove_edge).await.unwrap();
        assert_eq!(count(&store, "edges").await, 0);
    }

    #[tokio::test]
    async fn snapshot_and_subtree_persist_nothing() {
        let (store, _dir) = temp_store().await;
        let node = Node {
            id: "file:a.rs".to_string(),
            node_type: NodeType::File,
            label: "a.rs".to_string(),
            parent_id: None,
            child_ids: Vec::new(),
            status: NodeStatus::Unknown,
            docs: None,
            signature: None,
            meta: None,
        };
        let edge = Edge {
            id: "e:a->b".to_string(),
            source: "a".to_string(),
            target: "b".to_string(),
            kind: EdgeKind::Contains,
            hot: false,
        };
        let snapshot = EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: "sess-1".to_string(),
            event_type: EventType::Snapshot,
            payload: Payload::Snapshot {
                nodes: vec![node.clone()],
                edges: vec![edge.clone()],
            },
        };
        let subtree = EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: "sess-1".to_string(),
            event_type: EventType::Subtree,
            payload: Payload::Subtree {
                parent_id: "file:a.rs".to_string(),
                nodes: vec![node],
                edges: vec![edge],
            },
        };
        store.persist(&snapshot).await.unwrap();
        store.persist(&subtree).await.unwrap();
        for table in TABLES {
            assert_eq!(count(&store, table).await, 0, "view frame wrote to {table}");
        }
    }

    // ---- P8-3: persist the agent layer (DATA_MODEL §A.5 / §B.6) ----
    //
    // RED-phase contract for the agent-layer writers. P8-3 replaces the no-op
    // `Payload::AgentActivity | Payload::AgentRoster => {}` arm in `persist` with real
    // writes: `agent.roster` upserts real `agents` rows (true agent_type/color/status,
    // NOT the `#888888`/agent_id placeholders) + `protocol_versions` rows; `agent.activity`
    // inserts an `agent_activity` row (after upserting its `agents` parent for the FK).
    // These tests reference the existing no-op arm, so they COMPILE on the P8-1 base and
    // FAIL on assertions (the tables stay empty) until P8-3 lands. Each test asserts a
    // row COUNT before any `fetch_one` so the RED failure is a clean assertion, not a
    // panic on a missing row.

    /// P8-3 / AC1: an `agent.roster` writes one **real** `agents` row per `AgentInfo`,
    /// keyed by `process_id`, carrying the roster's true `agent_type`/`color`/`status`
    /// — never the `#888888` / agent-id placeholders the bare-pid `upsert_agent`
    /// stamps. RED until P8-3 replaces the no-op `AgentRoster` arm.
    #[tokio::test]
    async fn roster_writes_real_agent_row_per_agent() {
        let (store, _dir) = temp_store().await;
        let roster = agent_roster_env(
            "sess-1",
            vec![
                agent_info(
                    48213,
                    "tdd-green",
                    "implementation",
                    "#2ecc71",
                    "active",
                    None,
                ),
                agent_info(
                    48590,
                    "security-scanner",
                    "security",
                    "#e67e22",
                    "inactive",
                    None,
                ),
            ],
        );
        store.persist(&roster).await.expect("roster persist");

        // RED-safe: this fails first (0 != 2) before any row fetch could panic.
        assert_eq!(
            count(&store, "agents").await,
            2,
            "one agents row per AgentInfo, keyed by process_id"
        );

        let a = sqlx::query(
            "SELECT agent_id, agent_type, color, status FROM agents WHERE process_id = ?",
        )
        .bind(48213_i64)
        .fetch_one(&store.pool)
        .await
        .expect("agent 48213 row");
        let agent_id: String = a.get("agent_id");
        let agent_type: String = a.get("agent_type");
        let color: String = a.get("color");
        let status: String = a.get("status");
        assert_eq!(agent_id, "tdd-green");
        assert_eq!(agent_type, "implementation");
        assert_eq!(color, "#2ecc71");
        assert_eq!(status, "active");
        // The roster carries REAL metadata, not the bare-pid placeholders.
        assert_ne!(
            color, "#888888",
            "placeholder color leaked into a real roster row"
        );
        assert_ne!(
            agent_type, agent_id,
            "agent_type must be the real role, not the agent_id placeholder"
        );

        let b = sqlx::query(
            "SELECT agent_id, agent_type, color, status FROM agents WHERE process_id = ?",
        )
        .bind(48590_i64)
        .fetch_one(&store.pool)
        .await
        .expect("agent 48590 row");
        let agent_id: String = b.get("agent_id");
        let agent_type: String = b.get("agent_type");
        let color: String = b.get("color");
        let status: String = b.get("status");
        assert_eq!(agent_id, "security-scanner");
        assert_eq!(agent_type, "security");
        assert_eq!(color, "#e67e22");
        assert_eq!(status, "inactive");
    }

    /// P8-3 / AC2: re-persisting an `agent.roster` that flips a process to `inactive`
    /// updates that row's `status` via `ON CONFLICT(process_id)` — exactly one row
    /// survives for the process (no duplicate), reflecting the new status.
    #[tokio::test]
    async fn roster_reupsert_flips_status_on_conflict_no_duplicate() {
        let (store, _dir) = temp_store().await;
        store
            .persist(&agent_roster_env(
                "sess-1",
                vec![agent_info(
                    48213,
                    "tdd-green",
                    "implementation",
                    "#2ecc71",
                    "active",
                    None,
                )],
            ))
            .await
            .expect("first roster persist");
        store
            .persist(&agent_roster_env(
                "sess-1",
                vec![agent_info(
                    48213,
                    "tdd-green",
                    "implementation",
                    "#2ecc71",
                    "inactive",
                    None,
                )],
            ))
            .await
            .expect("second roster persist (status flip)");

        // RED-safe: this fails first (0 != 1) before the status fetch.
        assert_eq!(
            count(&store, "agents").await,
            1,
            "ON CONFLICT(process_id) keeps exactly one row per process"
        );
        let row = sqlx::query("SELECT status, updated_at FROM agents WHERE process_id = ?")
            .bind(48213_i64)
            .fetch_one(&store.pool)
            .await
            .expect("agent row");
        let status: String = row.get("status");
        assert_eq!(
            status, "inactive",
            "status must flip to inactive on re-roster"
        );
        let updated_at: String = row.get("updated_at");
        assert!(
            !updated_at.is_empty(),
            "updated_at must be refreshed on conflict"
        );
    }

    /// P8-3 / AC3: an `agent.roster` writes (or refreshes) one `protocol_versions` row
    /// per process — `version` from the `AgentInfo` (e.g. `"1"`), FK-valid against the
    /// `agents` rows the same roster upserts.
    #[tokio::test]
    async fn roster_writes_protocol_version_row_per_process() {
        let (store, _dir) = temp_store().await;
        let roster = agent_roster_env(
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
        );
        store.persist(&roster).await.expect("roster persist");

        // RED-safe: this fails first (0 != 2) before the version fetch.
        assert_eq!(
            count(&store, "protocol_versions").await,
            2,
            "one protocol_versions row per process in the roster"
        );
        let row =
            sqlx::query("SELECT version, session_id FROM protocol_versions WHERE process_id = ?")
                .bind(48213_i64)
                .fetch_one(&store.pool)
                .await
                .expect("protocol_versions row");
        let version: String = row.get("version");
        assert_eq!(version, "1");
        let session_id: String = row.get("session_id");
        assert_eq!(session_id, "sess-1");
        // FK integrity: every protocol_versions.process_id has an agents parent.
        let orphans: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM protocol_versions pv
             LEFT JOIN agents a ON a.process_id = pv.process_id
             WHERE a.process_id IS NULL",
        )
        .fetch_one(&store.pool)
        .await
        .expect("orphan count");
        assert_eq!(orphans, 0, "every protocol_versions row must be FK-valid");
    }

    /// P8-3 / decision: an `AgentInfo` whose `protocol_version` is `None` still persists a
    /// `protocol_versions` row with `version == "1"` — exercising the `unwrap_or("1")`
    /// default in [`upsert_protocol_version`], whose value was previously never asserted.
    #[tokio::test]
    async fn roster_without_protocol_version_defaults_version_to_one() {
        let (store, _dir) = temp_store().await;
        store
            .persist(&agent_roster_env(
                "sess-1",
                vec![agent_info(
                    48213,
                    "tdd-green",
                    "implementation",
                    "#2ecc71",
                    "active",
                    None,
                )],
            ))
            .await
            .expect("roster persist");

        // RED-safe: fails first (0 != 1) before the version fetch.
        assert_eq!(
            count(&store, "protocol_versions").await,
            1,
            "one protocol_versions row even when the roster omits protocolVersion"
        );
        let version: String =
            sqlx::query_scalar("SELECT version FROM protocol_versions WHERE process_id = ?")
                .bind(48213_i64)
                .fetch_one(&store.pool)
                .await
                .expect("protocol_versions version");
        assert_eq!(version, "1", "a missing protocolVersion defaults to \"1\"");
    }

    /// P8-3 / AC4: an `agent.activity` envelope writes exactly one `agent_activity` row
    /// and SUCCEEDS with no prior roster — proving the arm upserts the `agents` parent
    /// first (else the `process_id` FK rejects the insert). Each event adds one row.
    #[tokio::test]
    async fn agent_activity_writes_row_and_upserts_agent_first() {
        let (store, _dir) = temp_store().await;
        // No roster persisted first: the activity arm must upsert the agents row itself.
        store
            .persist(&agent_activity_env(
                "sess-1",
                "security-scanner",
                "modified",
                "fn:src/auth/token.rs:verify_token",
                Some(48590),
            ))
            .await
            .expect("agent.activity persist must satisfy the agents FK on its own");

        // RED-safe: this fails first (0 != 1) before any row fetch.
        assert_eq!(
            count(&store, "agent_activity").await,
            1,
            "one agent_activity row per agent.activity event"
        );
        assert_eq!(
            count(&store, "agents").await,
            1,
            "the activity arm upserts the agents parent (FK) before inserting"
        );
        let row = sqlx::query(
            "SELECT agent_id, action, node_id, process_id, session_id, ts FROM agent_activity",
        )
        .fetch_one(&store.pool)
        .await
        .expect("agent_activity row");
        let agent_id: String = row.get("agent_id");
        assert_eq!(agent_id, "security-scanner");
        let action: String = row.get("action");
        assert_eq!(action, "modified");
        let node_id: String = row.get("node_id");
        assert_eq!(node_id, "fn:src/auth/token.rs:verify_token");
        let pid: i64 = row.get("process_id");
        assert_eq!(pid, 48590);
        let session_id: String = row.get("session_id");
        assert_eq!(session_id, "sess-1");
        let ts: String = row.get("ts");
        assert!(!ts.is_empty(), "ts must be persisted");

        // Each event increments the row count by exactly one.
        store
            .persist(&agent_activity_env(
                "sess-1",
                "security-scanner",
                "modified",
                "fn:src/auth/token.rs:verify_token",
                Some(48590),
            ))
            .await
            .expect("second activity persist");
        assert_eq!(
            count(&store, "agent_activity").await,
            2,
            "row count increments by exactly one per event"
        );
    }

    /// P8-3 / decision-4: an `agent.activity` with NO `process_id` inserts exactly one
    /// `agent_activity` row carrying a NULL `process_id` (the relaxed FK) and creates NO
    /// `agents` row — the `if let Some(pid)` parent upsert is skipped and the NULL pid is
    /// bound, neither of which was previously asserted.
    #[tokio::test]
    async fn agent_activity_without_pid_writes_null_process_id_and_no_agent() {
        let (store, _dir) = temp_store().await;
        store
            .persist(&agent_activity_env(
                "sess-1",
                "security-scanner",
                "modified",
                "fn:a.rs:f",
                None,
            ))
            .await
            .expect("pid-less agent.activity persist");

        assert_eq!(
            count(&store, "agent_activity").await,
            1,
            "one agent_activity row for a pid-less activity"
        );
        assert_eq!(
            count(&store, "agents").await,
            0,
            "no agents row is upserted when the activity carries no pid"
        );
        let null_pids: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_activity WHERE process_id IS NULL")
                .fetch_one(&store.pool)
                .await
                .expect("null-pid count");
        assert_eq!(null_pids, 1, "the pid-less row stores a NULL process_id");
    }

    /// P8-3 / AC5 (retired invariant): Phase 7's
    /// `agent_activity_and_protocol_versions_stay_empty` asserted these two tables STAY
    /// EMPTY because no envelope wrote them. P8-3 adds the writers (`agent.activity` →
    /// `agent_activity`, `agent.roster` → `protocol_versions`), so that stay-empty
    /// invariant is **intentionally retired** for these two tables — this test now
    /// asserts the Phase-8 WRITE behaviour from a mixed batch. The pid (48213) is shared
    /// across the test result, roster, and activity so the roster REFINES the
    /// placeholder agent the test result seeded.
    #[tokio::test]
    async fn agent_activity_and_protocol_versions_written_in_phase_8() {
        let (store, _dir) = temp_store().await;
        let envelopes = vec![
            test_result_env("sess-1", Some(48213)),
            test_result_env("sess-1", None),
            node_upsert_env("f", NodeStatus::Unknown),
            edge_upsert_env(false),
            hot_edge_env("e:a->b", HotEdgeState::Enter),
            agent_roster_env(
                "sess-1",
                vec![agent_info(
                    48213,
                    "tdd-green",
                    "implementation",
                    "#2ecc71",
                    "active",
                    Some("1"),
                )],
            ),
            agent_activity_env("sess-1", "tdd-green", "modified", "fn:a.rs:f", Some(48213)),
        ];
        for env in &envelopes {
            store.persist(env).await.expect("persist");
        }
        assert_eq!(
            count(&store, "agent_activity").await,
            1,
            "P8-3 writes agent_activity (Phase-7 stay-empty invariant retired)"
        );
        assert_eq!(
            count(&store, "protocol_versions").await,
            1,
            "P8-3 writes protocol_versions (Phase-7 stay-empty invariant retired)"
        );
    }

    #[tokio::test]
    async fn raw_stdout_is_never_persisted() {
        // `persist` accepts only a typed `EventEnvelope` (the collector's parsed
        // output); a raw stdout line never reaches it. View frames write nothing, so
        // only structured, persistable events produce rows.
        let (store, _dir) = temp_store().await;
        let snapshot = EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: "sess-1".to_string(),
            event_type: EventType::Snapshot,
            payload: Payload::Snapshot {
                nodes: Vec::new(),
                edges: Vec::new(),
            },
        };
        store.persist(&snapshot).await.unwrap();
        for table in TABLES {
            assert_eq!(count(&store, table).await, 0, "view frame wrote to {table}");
        }
        store
            .persist(&test_result_env("sess-1", None))
            .await
            .unwrap();
        assert_eq!(
            count(&store, "test_results").await,
            1,
            "only structured events persist"
        );
    }

    /// P8-3 / AC5 (retired invariant): under P8-1 this test asserted the
    /// `agent.activity` / `agent.roster` arms were a no-op that wrote ZERO rows to every
    /// table. P8-3 replaces those no-op arms with real writes, so the "persist nothing"
    /// invariant is **intentionally retired** — this test now asserts the agent payloads
    /// PERSIST their rows (the direct contradiction of the old name). `protocol_versions`
    /// is left unasserted here (the roster's `AgentInfo` omits `protocolVersion`); AC3
    /// pins that table with an explicit version.
    #[tokio::test]
    async fn agent_activity_and_roster_payloads_persist_rows() {
        let (store, _dir) = temp_store().await;

        store
            .persist(&agent_activity_env(
                "sess-1",
                "security-scanner",
                "modified",
                "fn:a.rs:f",
                Some(48590),
            ))
            .await
            .expect("agent.activity persist");

        store
            .persist(&agent_roster_env(
                "sess-1",
                vec![agent_info(
                    48213,
                    "tdd-green",
                    "implementation",
                    "#2ecc71",
                    "active",
                    None,
                )],
            ))
            .await
            .expect("agent.roster persist");

        assert_eq!(
            count(&store, "agent_activity").await,
            1,
            "agent.activity now persists an agent_activity row (no-op arm retired)"
        );
        // The activity (pid 48590) and the roster (pid 48213) each upsert an agents row.
        assert_eq!(
            count(&store, "agents").await,
            2,
            "agent.activity + agent.roster each upsert their agents row"
        );
    }

    #[tokio::test]
    async fn record_session_writes_one_row() {
        let (store, _dir) = temp_store().await;
        store.record_session("run-1", "/repo").await.unwrap();
        let repo: String =
            sqlx::query_scalar("SELECT repo_path FROM sessions WHERE session_id = 'run-1'")
                .fetch_one(&store.pool)
                .await
                .expect("repo_path");
        assert_eq!(repo, "/repo");
        // Idempotent re-record.
        store.record_session("run-1", "/repo").await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_id = 'run-1'")
            .fetch_one(&store.pool)
            .await
            .expect("count");
        assert_eq!(n, 1);
    }

    // ---- P9-1: Storage read methods (load_nodes / load_edges) ----

    /// Wraps an arbitrary parsed [`Node`] in a `node.upsert` envelope stamped with
    /// `session`, so a parsed file can be persisted node-by-node (P9-1 load fixtures).
    fn node_upsert_for(session: &str, node: Node) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: session.to_string(),
            event_type: EventType::NodeUpsert,
            payload: Payload::NodeUpsert { node },
        }
    }

    /// Wraps an arbitrary parsed [`Edge`] in an `edge.upsert` envelope stamped with
    /// `session` (P9-1 load fixtures).
    fn edge_upsert_for(session: &str, edge: Edge) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: session.to_string(),
            event_type: EventType::EdgeUpsert,
            payload: Payload::EdgeUpsert { edge },
        }
    }

    /// Wraps a node id in a `node.remove` envelope stamped with `session`.
    fn node_remove_for(session: &str, id: &str) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            ts: "2026-06-30T00:00:00Z".to_string(),
            session_id: session.to_string(),
            event_type: EventType::NodeRemove,
            payload: Payload::NodeRemove { id: id.to_string() },
        }
    }

    /// Parses `(path, src)` and persists every node/edge under `session`, returning
    /// the parsed file so a test can derive the expected load result.
    async fn seed_parsed(store: &SqliteStore, session: &str, path: &str, src: &str) -> ParsedFile {
        let parsed = parse_source(path, src);
        for node in &parsed.nodes {
            store
                .persist(&node_upsert_for(session, node.clone()))
                .await
                .expect("persist node.upsert");
        }
        for edge in &parsed.edges {
            store
                .persist(&edge_upsert_for(session, edge.clone()))
                .await
                .expect("persist edge.upsert");
        }
        parsed
    }

    #[tokio::test]
    async fn load_methods_are_callable_through_trait_object() {
        // AC#1: load_nodes/load_edges keep the trait object-safe — they are callable
        // on a Box<dyn Storage + Send + Sync>, and an empty store yields empty Vecs.
        let (store, _dir) = temp_store().await;
        let dyn_store: Box<dyn Storage + Send + Sync> = Box::new(store);
        let nodes = dyn_store
            .load_nodes("sess-local")
            .await
            .expect("load_nodes");
        let edges = dyn_store
            .load_edges("sess-local")
            .await
            .expect("load_edges");
        assert!(nodes.is_empty(), "empty store yields no nodes");
        assert!(edges.is_empty(), "empty store yields no edges");
    }

    #[tokio::test]
    async fn load_nodes_round_trips_persisted_nodes_with_empty_child_ids() {
        // AC#2: every persisted node loads back with id/type/label/parentId/status
        // intact across a multi-file source set; child_ids is NOT persisted, so it
        // loads as [] (re-derived later by Graph::from_records, not by load_nodes).
        let (store, _dir) = temp_store().await;
        let mut expected: Vec<Node> = Vec::new();
        expected.extend(
            seed_parsed(&store, "sess-local", "a.rs", "fn alpha() { let x = 1; }")
                .await
                .nodes,
        );
        expected.extend(
            seed_parsed(&store, "sess-local", "b.rs", "fn beta() {}")
                .await
                .nodes,
        );

        let loaded = store.load_nodes("sess-local").await.expect("load_nodes");
        let by_id: std::collections::HashMap<&str, &Node> =
            loaded.iter().map(|n| (n.id.as_str(), n)).collect();
        assert_eq!(
            loaded.len(),
            expected.len(),
            "every persisted node loads: {loaded:?}"
        );
        for want in &expected {
            let got = by_id
                .get(want.id.as_str())
                .unwrap_or_else(|| panic!("missing node {}", want.id));
            assert_eq!(got.node_type, want.node_type, "type for {}", want.id);
            assert_eq!(got.label, want.label, "label for {}", want.id);
            assert_eq!(got.parent_id, want.parent_id, "parentId for {}", want.id);
            assert_eq!(got.status, want.status, "status for {}", want.id);
            // Finding #4: the JSON-encoded signature/meta columns must round-trip back
            // equal through load_nodes (the function node `fn:a.rs:alpha` carries both).
            assert_eq!(got.signature, want.signature, "signature for {}", want.id);
            assert_eq!(got.meta, want.meta, "meta for {}", want.id);
            assert!(
                got.child_ids.is_empty(),
                "child_ids is unpersisted and must load as [] for {}: {:?}",
                want.id,
                got.child_ids
            );
        }
        // Guard: the fixture really did persist a non-None signature AND meta, so the
        // round-trip assertions above are meaningful (not vacuously comparing None==None).
        let alpha = by_id
            .get("fn:a.rs:alpha")
            .expect("function node fn:a.rs:alpha must load");
        assert!(
            alpha.signature.is_some(),
            "fixture function must carry a signature to exercise the round-trip"
        );
        assert!(
            alpha.meta.is_some(),
            "fixture function must carry meta to exercise the round-trip"
        );
    }

    #[tokio::test]
    async fn load_edges_round_trips_persisted_edges() {
        // AC#2: every persisted edge loads back with source/target/kind intact.
        let (store, _dir) = temp_store().await;
        let mut expected: Vec<Edge> = Vec::new();
        expected.extend(
            seed_parsed(&store, "sess-local", "a.rs", "fn alpha() { let x = 1; }")
                .await
                .edges,
        );
        expected.extend(
            seed_parsed(&store, "sess-local", "b.rs", "fn beta() {}")
                .await
                .edges,
        );

        let loaded = store.load_edges("sess-local").await.expect("load_edges");
        let by_id: std::collections::HashMap<&str, &Edge> =
            loaded.iter().map(|e| (e.id.as_str(), e)).collect();
        assert_eq!(
            loaded.len(),
            expected.len(),
            "every persisted edge loads: {loaded:?}"
        );
        for want in &expected {
            let got = by_id
                .get(want.id.as_str())
                .unwrap_or_else(|| panic!("missing edge {}", want.id));
            assert_eq!(got.source, want.source, "source for {}", want.id);
            assert_eq!(got.target, want.target, "target for {}", want.id);
            assert_eq!(got.kind, want.kind, "kind for {}", want.id);
        }
    }

    #[tokio::test]
    async fn load_nodes_excludes_a_removed_node() {
        // AC#2: a node.remove-then-load does not return the removed node.
        let (store, _dir) = temp_store().await;
        let parsed = seed_parsed(&store, "sess-local", "a.rs", "fn alpha() {}\nfn beta() {}").await;
        assert!(
            parsed.nodes.iter().any(|n| n.id == "fn:a.rs:beta"),
            "fixture must seed fn:a.rs:beta"
        );

        store
            .persist(&node_remove_for("sess-local", "fn:a.rs:beta"))
            .await
            .expect("persist node.remove");

        let loaded = store.load_nodes("sess-local").await.expect("load_nodes");
        assert!(
            loaded.iter().all(|n| n.id != "fn:a.rs:beta"),
            "removed node must not load: {loaded:?}"
        );
        assert!(
            loaded.iter().any(|n| n.id == "fn:a.rs:alpha"),
            "the surviving node must still load: {loaded:?}"
        );
    }
}
