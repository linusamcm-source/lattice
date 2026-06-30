//! Persistence seam — the write-through storage contract (`DATA_MODEL.md` §B).
//!
//! Lattice keeps the in-memory [`crate::graph::Graph`] as the live source of truth
//! for snapshots and subtrees; this module is the **write-through** persistence
//! alongside it. A run durably records the structured CLV event stream — the same
//! [`EventEnvelope`]s that flow on the broadcast channel — so a session's history is
//! queryable later (`DATA_MODEL.md` §B.5: only parsed, structured CLV events are
//! persisted; raw stdout stays ephemeral).
//!
//! Persistence is **opt-in via `LATTICE_DB_URL`**: when it is unset the backend runs
//! exactly as before with no database. When set, its URL scheme selects one of two
//! interchangeable `sqlx` backends behind a single async [`Storage`] trait —
//! **SQLite** (`sqlite:`, solo/local) or **Postgres** (`postgres:`/`postgresql:`,
//! team) — per `DATA_MODEL.md` §B's "same schema, only the driver changes". The
//! relational schema (the seven tables of §B.6) is created idempotently by
//! [`Storage::ensure_schema`].
//!
//! Story P7-1 laid the contract: the [`Storage`] trait, the [`StorageError`] type,
//! the [`Backend`] scheme classifier ([`backend_for_url`]), and the [`open_store`]
//! factory. Story P7-2 adds the **live SQLite backend** in the [`sqlite`] submodule
//! ([`sqlite::SqliteStore`]) and wires it into [`open_store`]'s `sqlite:` arm; the
//! Postgres twin (P7-3) still returns a [`StorageError::Config`] rather than
//! panicking until implemented.
//!
//! The backends use `sqlx`'s **runtime** query API (not the compile-time `query!`
//! macro), so the build and `just qg` stay hermetic — no `DATABASE_URL` and no live
//! database are required to compile or test.

mod sqlite;

use crate::wire::EventEnvelope;

/// An error from the persistence layer (`DATA_MODEL.md` §B).
///
/// Two variants cover the only ways storage can fail: a database/driver error
/// surfaced by `sqlx`, and a configuration error such as an unrecognised
/// `LATTICE_DB_URL` scheme. Implements [`std::error::Error`] (chaining to the
/// underlying `sqlx::Error` via [`std::error::Error::source`]) so callers can treat
/// it like any other boxed error.
#[derive(Debug)]
pub enum StorageError {
    /// A failure from the underlying `sqlx` driver (connect, schema, or query).
    Db(sqlx::Error),
    /// A configuration problem — e.g. an unknown/malformed `LATTICE_DB_URL`
    /// scheme, or a backend that is not yet implemented.
    Config(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::Db(err) => write!(f, "storage backend error: {err}"),
            StorageError::Config(msg) => write!(f, "storage configuration error: {msg}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Db(err) => Some(err),
            StorageError::Config(_) => None,
        }
    }
}

impl From<sqlx::Error> for StorageError {
    fn from(err: sqlx::Error) -> Self {
        StorageError::Db(err)
    }
}

/// The database backend selected by a `LATTICE_DB_URL` scheme (`DATA_MODEL.md` §B).
///
/// Per §B the two backends share one schema and differ only in their `sqlx` driver:
/// [`Backend::Sqlite`] for the solo/local file database, [`Backend::Postgres`] for a
/// shared team instance. [`backend_for_url`] maps a URL to one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// SQLite (`sqlite:` / `sqlite://…`) — solo/local file or in-memory database.
    Sqlite,
    /// Postgres (`postgres://…` / `postgresql://…`) — shared team instance.
    Postgres,
}

/// Classifies a `LATTICE_DB_URL` into the [`Backend`] that should serve it.
///
/// Implements the `DATA_MODEL.md` §B scheme selection: a `sqlite:` URL maps to
/// [`Backend::Sqlite`] and a `postgres:`/`postgresql:` URL to [`Backend::Postgres`].
/// Any other scheme, or a URL with no scheme at all, is a configuration error and
/// yields [`StorageError::Config`] naming the offending scheme/URL (never a panic),
/// honouring the never-panic-on-bad-input contract.
///
/// ```
/// use lattice_backend::storage::{backend_for_url, Backend};
/// assert_eq!(backend_for_url("sqlite://./.lattice/graph.db").ok(), Some(Backend::Sqlite));
/// assert_eq!(backend_for_url("postgres://u@h/db").ok(), Some(Backend::Postgres));
/// assert!(backend_for_url("mysql://u@h/db").is_err());
/// ```
pub fn backend_for_url(url: &str) -> Result<Backend, StorageError> {
    // URI schemes are case-insensitive (RFC 3986 §3.1), and a `LATTICE_DB_URL` env
    // value may carry stray surrounding whitespace — normalise both before matching.
    match url.trim().split_once(':') {
        Some((scheme, _)) => match scheme.to_ascii_lowercase().as_str() {
            "sqlite" => Ok(Backend::Sqlite),
            "postgres" | "postgresql" => Ok(Backend::Postgres),
            _ => Err(StorageError::Config(format!(
                "unsupported LATTICE_DB_URL scheme '{scheme}' (expected sqlite or postgres)"
            ))),
        },
        None => Err(StorageError::Config(format!(
            "malformed LATTICE_DB_URL (no scheme): '{url}'"
        ))),
    }
}

/// The persistence contract — write-through storage of structured CLV events
/// (`DATA_MODEL.md` §B).
///
/// Each method maps onto the §B relational model: [`ensure_schema`] creates the §B.6
/// seven-table schema idempotently, [`persist`] write-throughs one structured
/// [`EventEnvelope`] (§B.5 — only structured events, never raw stdout), and
/// [`record_session`] inserts the run's `sessions` row (§B.6 `sessions`). It is
/// annotated `#[async_trait::async_trait]` so it stays **object-safe**: the factory
/// hands callers a `Box<dyn Storage + Send + Sync>`, which native async-fn-in-trait
/// does not yet support on stable Rust. Implementations use `sqlx`'s runtime query
/// API so no database is needed to build or test the contract itself.
///
/// [`ensure_schema`]: Storage::ensure_schema
/// [`persist`]: Storage::persist
/// [`record_session`]: Storage::record_session
#[async_trait::async_trait]
pub trait Storage: Send + Sync {
    /// Creates the `DATA_MODEL.md` §B.6 schema idempotently (`CREATE TABLE IF NOT
    /// EXISTS`), so calling it more than once on the same database is a no-op.
    /// Called once when the store is opened.
    async fn ensure_schema(&self) -> Result<(), StorageError>;

    /// Write-throughs one structured [`EventEnvelope`] to its `DATA_MODEL.md` §B
    /// row(s).
    ///
    /// Only structured CLV events reach this method (§B.5); raw stdout is never
    /// persisted. View-only frames (`snapshot`/`subtree`) persist nothing. The
    /// per-payload write rules are implemented by each backend.
    async fn persist(&self, env: &EventEnvelope) -> Result<(), StorageError>;

    /// Upserts the run's `sessions` row (`DATA_MODEL.md` §B.6 `sessions`) keyed by
    /// `session_id` and recording the watched `repo_path`, satisfying the
    /// `REFERENCES sessions(session_id)` foreign keys of the event tables.
    async fn record_session(&self, session_id: &str, repo_path: &str) -> Result<(), StorageError>;
}

/// Opens the [`Storage`] backend selected by a `LATTICE_DB_URL` (`DATA_MODEL.md` §B).
///
/// Dispatches on [`backend_for_url`]: a `sqlite:` URL opens the **live** SQLite
/// backend ([`sqlite::SqliteStore`], story P7-2) — connecting the pool with
/// `PRAGMA foreign_keys = ON` and creating the database file if missing — while a
/// `postgres:`/`postgresql:` URL is not yet implemented (story P7-3) and returns a
/// [`StorageError::Config`] rather than panicking. Opening does **not** create the
/// schema; the caller invokes [`Storage::ensure_schema`] (and
/// [`Storage::record_session`]) once after opening. A connect failure surfaces as a
/// [`StorageError::Db`]; an unknown or malformed scheme propagates the
/// [`backend_for_url`] error.
pub async fn open_store(url: &str) -> Result<Box<dyn Storage + Send + Sync>, StorageError> {
    match backend_for_url(url)? {
        Backend::Sqlite => Ok(Box::new(sqlite::SqliteStore::connect(url).await?)),
        Backend::Postgres => Err(StorageError::Config(
            "postgres backend not yet implemented (Phase 7 story P7-3)".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_for_url_classifies_scheme() {
        let cases: Vec<(&str, Result<Backend, ()>)> = vec![
            ("sqlite:graph.db", Ok(Backend::Sqlite)),
            ("sqlite://./.lattice/graph.db", Ok(Backend::Sqlite)),
            (
                "postgres://user:pass@host:5432/lattice",
                Ok(Backend::Postgres),
            ),
            (
                "postgresql://user:pass@host:5432/lattice",
                Ok(Backend::Postgres),
            ),
            ("mysql://user:pass@host/db", Err(())),
            ("graph.db", Err(())),
            // case-insensitive scheme (RFC 3986) + surrounding whitespace are normalised
            ("SQLITE://x", Ok(Backend::Sqlite)),
            ("Postgres://x", Ok(Backend::Postgres)),
            ("  sqlite:graph.db  ", Ok(Backend::Sqlite)),
        ];
        for (url, want) in cases {
            match (backend_for_url(url), want) {
                (Ok(got), Ok(expected)) => assert_eq!(got, expected, "url {url}"),
                (Err(_), Err(())) => {}
                (got, want) => panic!("url {url}: got {got:?}, want {want:?}"),
            }
        }
    }

    struct DummyStore;

    #[async_trait::async_trait]
    impl Storage for DummyStore {
        async fn ensure_schema(&self) -> Result<(), StorageError> {
            Ok(())
        }
        async fn persist(&self, _env: &EventEnvelope) -> Result<(), StorageError> {
            Ok(())
        }
        async fn record_session(
            &self,
            _session_id: &str,
            _repo_path: &str,
        ) -> Result<(), StorageError> {
            Ok(())
        }
    }

    #[test]
    fn storage_trait_is_object_safe() {
        let _b: Box<dyn Storage + Send + Sync> = Box::new(DummyStore);
    }

    #[tokio::test]
    async fn open_store_postgres_arm_still_returns_config_error() {
        // The Postgres arm must error (not panic) until P7-3 lands.
        match open_store("postgres://u@h/db").await {
            Err(StorageError::Config(_)) => {}
            Ok(_) => panic!("postgres unexpectedly opened a store"),
            Err(other) => panic!("expected Config err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_store_opens_live_sqlite_backend() {
        // The SQLite arm (P7-2) opens a live store and applies the schema, rather
        // than returning a Config error.
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}", dir.path().join("open.db").display());
        let store = open_store(&url).await.expect("open sqlite store");
        store.ensure_schema().await.expect("ensure schema");
    }

    #[tokio::test]
    async fn open_store_propagates_unknown_scheme_error() {
        match open_store("mysql://u@h/db").await {
            Err(StorageError::Config(_)) => {}
            Ok(_) => panic!("unknown scheme unexpectedly opened a store"),
            Err(other) => panic!("expected Config err, got {other:?}"),
        }
    }

    #[test]
    fn storage_error_display_and_source() {
        let config = StorageError::Config("bad scheme".to_string());
        assert!(config.to_string().contains("bad scheme"));
        assert!(std::error::Error::source(&config).is_none());

        let db: StorageError = sqlx::Error::RowNotFound.into();
        assert!(db.to_string().contains("storage backend error"));
        assert!(std::error::Error::source(&db).is_some());
    }
}
