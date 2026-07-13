//! Independent, read-only view of the SQLite database written by `warden`.
//!
//! Deliberately **duplicated** from (rather than importing) `warden`'s own
//! `db.rs` -- same rationale `warden_gated::db` already documents
//! (Architecture.md §13, ADR-0006): `warden-tui` must never inherit a bug in
//! `warden`'s own query/parsing logic, and the two crates only share
//! `warden-core` (types), never each other's I/O code. `warden` is the sole
//! writer (code-standards.md, "SQLite & sqlx") -- this module only ever
//! opens the database read-only and never runs migrations.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use warden_core::{EventKind, RunEvent, RunEventRecord, RunState};

use crate::error::{Result, TuiError};

/// Matches `warden::db`'s own busy timeout: this read-only connection can
/// still contend with `warden`'s writer under WAL.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens `db_path` strictly **read-only**. Fails loudly rather than
/// creating the file -- an absent database means a misconfigured path or a
/// `warden` that has never run, never a case to silently paper over.
pub async fn connect_read_only(db_path: &Path) -> Result<SqlitePool> {
    if !db_path.exists() {
        return Err(TuiError::DatabaseNotFound(db_path.to_path_buf()));
    }

    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .read_only(true)
        .busy_timeout(BUSY_TIMEOUT);

    let pool = SqlitePoolOptions::new().connect_with(options).await?;
    Ok(pool)
}

/// The subset of a `runs` row the TUI header needs to render. Re-read from
/// SQLite on demand, never trusted from a cached value that might have
/// drifted (same discipline `warden_gated::db::GateRunView` follows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunView {
    pub id: String,
    pub intent: String,
    pub branch: String,
    pub state: RunState,
    pub max_cycles: u32,
    pub current_cycle: u32,
}

struct RunRow {
    id: String,
    intent: String,
    branch: String,
    state: String,
    max_cycles: i64,
    current_cycle: i64,
}

fn checked_u32(value: i64, column: &'static str) -> Result<u32> {
    u32::try_from(value).map_err(|_| TuiError::InvalidStoredValue { column, value })
}

pub async fn get_run(pool: &SqlitePool, run_id: &str) -> Result<Option<RunView>> {
    let row = sqlx::query_as!(
        RunRow,
        r#"SELECT id as "id!", intent, branch, state, max_cycles, current_cycle FROM runs WHERE id = ?"#,
        run_id,
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| -> Result<RunView> {
        Ok(RunView {
            id: r.id,
            intent: r.intent,
            branch: r.branch,
            state: RunState::parse(&r.state)?,
            max_cycles: checked_u32(r.max_cycles, "runs.max_cycles")?,
            current_cycle: checked_u32(r.current_cycle, "runs.current_cycle")?,
        })
    })
    .transpose()
}

struct EventRow {
    id: String,
    run_id: String,
    event_type: String,
    payload_json: String,
    created_at: String,
}

fn row_to_event_record(row: EventRow) -> Result<RunEventRecord> {
    let declared_kind = EventKind::parse(&row.event_type)?;
    let event: RunEvent = serde_json::from_str(&row.payload_json)?;
    if event.kind() != declared_kind {
        return Err(TuiError::EventKindMismatch {
            id: row.id,
            event_type: row.event_type,
            payload_kind: event.kind().as_str(),
        });
    }
    Ok(RunEventRecord {
        id: row.id,
        run_id: row.run_id,
        event,
        created_at: row.created_at,
    })
}

/// Every event recorded for `run_id`, oldest first -- the history a late
/// attach replays before switching to the live socket stream
/// (Architecture.md §5.4). Mirrors `warden::db::list_events_for_run`
/// exactly (independently re-implemented, per this module's doc comment).
pub async fn list_events_for_run(pool: &SqlitePool, run_id: &str) -> Result<Vec<RunEventRecord>> {
    let rows = sqlx::query_as!(
        EventRow,
        r#"
        SELECT id as "id!", run_id, event_type, payload_json, created_at
        FROM events
        WHERE run_id = ?
        ORDER BY created_at ASC, id ASC
        "#,
        run_id,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(row_to_event_record).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteConnectOptions as WriteOptions;
    use tempfile::TempDir;

    #[tokio::test]
    async fn connect_read_only_fails_loudly_when_the_database_does_not_exist() {
        let dir = TempDir::new().unwrap();
        let missing_db = dir.path().join("does-not-exist.db");

        let result = connect_read_only(&missing_db).await;
        assert!(matches!(result, Err(TuiError::DatabaseNotFound(_))));
    }

    /// Sets up a real SQLite file with the two tables this crate reads,
    /// via a plain `CREATE TABLE` (this crate must not depend on `warden`'s
    /// migrations either, same reasoning as `warden_gated::db`'s tests).
    async fn seed_db(dir: &Path) -> (std::path::PathBuf, SqlitePool) {
        let db_path = dir.join("state.db");
        let write_options = WriteOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let write_pool = SqlitePoolOptions::new()
            .connect_with(write_options)
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE runs (
                id TEXT PRIMARY KEY,
                intent TEXT NOT NULL,
                branch TEXT NOT NULL,
                state TEXT NOT NULL,
                max_cycles INTEGER NOT NULL,
                current_cycle INTEGER NOT NULL
            )",
        )
        .execute(&write_pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE events (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            )",
        )
        .execute(&write_pool)
        .await
        .unwrap();

        (db_path, write_pool)
    }

    #[tokio::test]
    async fn get_run_round_trips_a_seeded_row() {
        let dir = TempDir::new().unwrap();
        let (db_path, write_pool) = seed_db(dir.path()).await;
        sqlx::query(
            "INSERT INTO runs (id, intent, branch, state, max_cycles, current_cycle) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("run-1")
        .bind("do the thing")
        .bind("main")
        .bind("coder_running")
        .bind(5)
        .bind(1)
        .execute(&write_pool)
        .await
        .unwrap();
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let run = get_run(&pool, "run-1")
            .await
            .unwrap()
            .expect("run-1 exists");
        assert_eq!(run.intent, "do the thing");
        assert_eq!(run.state, RunState::CoderRunning);
        assert_eq!(run.max_cycles, 5);
        assert_eq!(run.current_cycle, 1);
    }

    #[tokio::test]
    async fn get_run_returns_none_for_an_unknown_run() {
        let dir = TempDir::new().unwrap();
        let (db_path, write_pool) = seed_db(dir.path()).await;
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let run = get_run(&pool, "does-not-exist").await.unwrap();
        assert!(run.is_none());
    }

    #[tokio::test]
    async fn list_events_for_run_round_trips_and_orders_oldest_first() {
        let dir = TempDir::new().unwrap();
        let (db_path, write_pool) = seed_db(dir.path()).await;

        let insert = |id: &'static str,
                      event_type: &'static str,
                      payload: String,
                      created_at: &'static str| {
            let write_pool = write_pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(id)
                .bind("run-1")
                .bind(event_type)
                .bind(payload)
                .bind(created_at)
                .execute(&write_pool)
                .await
                .unwrap();
            }
        };

        let later = serde_json::to_string(&RunEvent::CycleStarted { cycle_number: 2 }).unwrap();
        let earlier = serde_json::to_string(&RunEvent::CycleStarted { cycle_number: 1 }).unwrap();
        insert(
            "event-b",
            "cycle_started",
            later,
            "2026-07-12T00:00:02+00:00",
        )
        .await;
        insert(
            "event-a",
            "cycle_started",
            earlier,
            "2026-07-12T00:00:01+00:00",
        )
        .await;
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let events = list_events_for_run(&pool, "run-1").await.unwrap();
        let ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["event-a", "event-b"]);
    }

    #[tokio::test]
    async fn mismatched_event_type_and_payload_kind_is_a_typed_error() {
        let dir = TempDir::new().unwrap();
        let (db_path, write_pool) = seed_db(dir.path()).await;
        let payload = serde_json::to_string(&RunEvent::CycleStarted { cycle_number: 1 }).unwrap();
        sqlx::query(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("event-corrupt")
        .bind("run-1")
        .bind("run_finished")
        .bind(payload)
        .bind("2026-07-12T00:00:00+00:00")
        .execute(&write_pool)
        .await
        .unwrap();
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let result = list_events_for_run(&pool, "run-1").await;
        assert!(matches!(result, Err(TuiError::EventKindMismatch { .. })));
    }
}
