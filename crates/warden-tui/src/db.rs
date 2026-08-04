//! Independent, read-only view of the SQLite database written by `warden`.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use warden_core::{
    EventKind, ProgressReplay, RunEvent, RunEventHistoryEntry, RunEventRecord, RunState,
    UndecodableEvent, UndecodableReason,
};

use crate::error::{Result, TuiError};

/// Matches `warden::db`'s own busy timeout: this read-only connection can still contend with
/// `warden`'s writer under WAL.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens `db_path` strictly **read-only**.
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

/// The subset of a `runs` row the TUI header needs to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunView {
    pub id: String,
    pub intent: String,
    pub branch: String,
    pub state: RunState,
    pub max_cycles: u32,
    pub current_cycle: u32,
}

#[derive(sqlx::FromRow)]
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
    let row = sqlx::query_as::<_, RunRow>(
        "SELECT id, intent, branch, state, max_review_cycles AS max_cycles, \
         current_review_cycle AS current_cycle FROM runs WHERE id = ?",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;

    row.map(|r| -> Result<RunView> {
        Ok(RunView {
            id: r.id,
            intent: r.intent,
            branch: r.branch,
            state: RunState::parse(&r.state)?,
            max_cycles: checked_u32(r.max_cycles, "runs.max_review_cycles")?,
            current_cycle: checked_u32(r.current_cycle, "runs.current_review_cycle")?,
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

/// Validates one row's `event_type`/`payload_json` into a [`RunEventHistoryEntry`].
fn row_to_history_entry(row: EventRow) -> RunEventHistoryEntry {
    let reason = match EventKind::parse(&row.event_type) {
        Ok(declared_kind) => match serde_json::from_str::<RunEvent>(&row.payload_json) {
            Ok(event) if event.kind() == declared_kind => {
                return RunEventHistoryEntry::Decoded(RunEventRecord {
                    id: row.id,
                    run_id: row.run_id,
                    event,
                    created_at: row.created_at,
                });
            }
            Ok(event) => UndecodableReason::KindMismatch {
                payload_kind: event.kind().as_str().to_string(),
            },
            Err(_) => UndecodableReason::PayloadDeserialize,
        },
        Err(_) => UndecodableReason::UnknownEventType,
    };
    RunEventHistoryEntry::Undecodable(UndecodableEvent {
        id: row.id,
        run_id: row.run_id,
        event_type: row.event_type,
        reason,
        created_at: row.created_at,
    })
}

/// Every event recorded for `run_id`, oldest first -- the history a late attach replays before
/// switching to the live socket stream (Architecture.md §5.4).
///
/// `progress` decides whether `agent_progress` rows are part of that history; they are not, unless
/// the caller opts in (issue #108, see [`ProgressReplay`]). With
/// [`ProgressReplay::Included`], the replayed sequence is exactly what a subscriber connected from
/// the start saw live, in publication order -- up to the per-step persistence cap `warden` applies
/// when writing.
///
/// **Ordering.** `created_at ASC` is publication order (the timestamp is stamped at publication,
/// not at write), and `rowid ASC` breaks a tie in `warden`'s own insertion order -- deterministic,
/// where the previous `id ASC` fallback ordered by a random UUID. Mirrors `warden::db`'s own query
/// deliberately: this reader is an independent duplicate, never a shared code path (ADR-0006).
pub async fn list_events_for_run(
    pool: &SqlitePool,
    run_id: &str,
    progress: ProgressReplay,
) -> Result<Vec<RunEventHistoryEntry>> {
    let excluded_kind = EventKind::AgentProgress.as_str();
    let rows = if progress.includes_progress() {
        sqlx::query_as!(
            EventRow,
            r#"
            SELECT id as "id!", run_id, event_type, payload_json, created_at
            FROM events
            WHERE run_id = ?
            ORDER BY created_at ASC, rowid ASC
            "#,
            run_id,
        )
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as!(
            EventRow,
            r#"
            SELECT id as "id!", run_id, event_type, payload_json, created_at
            FROM events
            WHERE run_id = ? AND event_type <> ?
            ORDER BY created_at ASC, rowid ASC
            "#,
            run_id,
            excluded_kind,
        )
        .fetch_all(pool)
        .await?
    };

    Ok(rows.into_iter().map(row_to_history_entry).collect())
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
                max_review_cycles INTEGER NOT NULL,
                max_test_cycles INTEGER NOT NULL,
                current_review_cycle INTEGER NOT NULL,
                current_test_cycle INTEGER NOT NULL
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
            "INSERT INTO runs (id, intent, branch, state, max_review_cycles, max_test_cycles, current_review_cycle, current_test_cycle) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("run-1")
        .bind("do the thing")
        .bind("main")
        .bind("coder_running")
        .bind(5)
        .bind(4)
        .bind(1)
        .bind(0)
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
        assert_eq!(run.state, RunState::RunningStep(0));
        assert_eq!(run.max_cycles, 5);
        assert_eq!(run.current_cycle, 1);
    }

    /// ADR-0008: the TUI observes, it never acts. A pool that merely *intends* to be read-only is
    /// not a guarantee -- pin that SQLite itself refuses a write through it, so no future feature
    /// (issue #107's workflow graph included) can quietly gain a mutation path.
    #[tokio::test]
    async fn a_read_only_pool_refuses_to_write_to_the_database() {
        let dir = TempDir::new().unwrap();
        let (db_path, write_pool) = seed_db(dir.path()).await;
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let error = sqlx::query(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) \
             VALUES ('x', 'run-1', 'run_started', '{}', '2026-07-12T00:00:00+00:00')",
        )
        .execute(&pool)
        .await
        .expect_err("a read-only connection must reject an INSERT");
        assert!(
            error.to_string().to_lowercase().contains("readonly")
                || error.to_string().to_lowercase().contains("read-only"),
            "expected a read-only rejection, got: {error}"
        );

        let deletion = sqlx::query("DELETE FROM runs").execute(&pool).await;
        assert!(
            deletion.is_err(),
            "a read-only connection must reject a DELETE"
        );
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
        let events = list_events_for_run(&pool, "run-1", ProgressReplay::Included)
            .await
            .unwrap();
        let ids: Vec<&str> = events.iter().map(|e| e.id()).collect();
        assert_eq!(ids, vec!["event-a", "event-b"]);
    }

    /// Issue #108: `agent_progress` outnumbers every other kind by an order of magnitude, so a
    /// replay leaves it out unless the reader asks for it -- and when it does ask, gets it back
    /// interleaved in publication order, not appended at the end.
    #[tokio::test]
    async fn agent_progress_is_excluded_from_replay_by_default_and_returned_on_opt_in() {
        let dir = TempDir::new().unwrap();
        let (db_path, write_pool) = seed_db(dir.path()).await;

        let rows = [
            (
                "e1",
                RunEvent::AgentStarted {
                    role: "implementation".to_string(),
                },
            ),
            (
                "e2",
                RunEvent::AgentProgress {
                    role: "implementation".to_string(),
                    detail: "message: reading src/lib.rs".to_string(),
                },
            ),
            (
                "e3",
                RunEvent::AgentProgress {
                    role: "implementation".to_string(),
                    detail: "tool: Edit".to_string(),
                },
            ),
            (
                "e4",
                RunEvent::AgentFinished {
                    role: "implementation".to_string(),
                    exit_code: 0,
                    usage: None,
                },
            ),
        ];
        for (index, (id, event)) in rows.iter().enumerate() {
            sqlx::query(
                "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, 'run-1', ?, ?, ?)",
            )
            .bind(id)
            .bind(event.kind().as_str())
            .bind(serde_json::to_string(event).unwrap())
            .bind(format!("2026-08-04T00:00:0{index}+00:00"))
            .execute(&write_pool)
            .await
            .unwrap();
        }
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();

        let default_replay = list_events_for_run(&pool, "run-1", ProgressReplay::Excluded)
            .await
            .unwrap();
        assert_eq!(
            default_replay.iter().map(|e| e.id()).collect::<Vec<_>>(),
            vec!["e1", "e4"],
            "a default attach must replay the lifecycle events and nothing else"
        );

        let opted_in = list_events_for_run(&pool, "run-1", ProgressReplay::Included)
            .await
            .unwrap();
        assert_eq!(
            opted_in.iter().map(|e| e.id()).collect::<Vec<_>>(),
            vec!["e1", "e2", "e3", "e4"],
            "--include-progress must replay progress where it was published"
        );
    }

    #[tokio::test]
    async fn mismatched_event_type_and_payload_kind_is_an_undecodable_entry_not_a_failed_query() {
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
        let events = list_events_for_run(&pool, "run-1", ProgressReplay::Included)
            .await
            .expect("one bad row must never fail the whole query");
        assert_eq!(events.len(), 1);
        match &events[0] {
            RunEventHistoryEntry::Undecodable(event) => {
                assert_eq!(event.id, "event-corrupt");
                assert_eq!(event.event_type, "run_finished");
                assert_eq!(
                    event.reason,
                    UndecodableReason::KindMismatch {
                        payload_kind: "cycle_started".to_string()
                    }
                );
            }
            RunEventHistoryEntry::Decoded(record) => {
                panic!("expected an Undecodable entry, got a decoded record: {record:?}")
            }
        }
    }

    #[tokio::test]
    async fn history_with_a_malformed_payload_and_a_kind_mismatch_still_returns_every_good_event() {
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

        insert(
            "event-good-1",
            "run_started",
            serde_json::to_string(&RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_cycles: 3,
            })
            .unwrap(),
            "2026-07-12T00:00:00+00:00",
        )
        .await;
        insert(
            "event-malformed",
            "cycle_started",
            "{ not json".to_string(),
            "2026-07-12T00:00:01+00:00",
        )
        .await;
        insert(
            "event-mismatched",
            "run_finished",
            serde_json::to_string(&RunEvent::CycleStarted { cycle_number: 1 }).unwrap(),
            "2026-07-12T00:00:02+00:00",
        )
        .await;
        insert(
            "event-good-2",
            "cycle_started",
            serde_json::to_string(&RunEvent::CycleStarted { cycle_number: 2 }).unwrap(),
            "2026-07-12T00:00:03+00:00",
        )
        .await;
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let events = list_events_for_run(&pool, "run-1", ProgressReplay::Included)
            .await
            .expect("undecodable rows must never fail the whole query");

        assert_eq!(events.len(), 4, "{events:?}");
        let ids: Vec<&str> = events.iter().map(|e| e.id()).collect();
        assert_eq!(
            ids,
            vec![
                "event-good-1",
                "event-malformed",
                "event-mismatched",
                "event-good-2",
            ],
            "order (created_at ASC, id ASC) must be preserved even with bad rows interleaved"
        );

        assert!(matches!(
            events[0],
            RunEventHistoryEntry::Decoded(ref record) if record.event == RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_cycles: 3,
            }
        ));
        assert!(matches!(
            events[1],
            RunEventHistoryEntry::Undecodable(ref event) if event.event_type == "cycle_started"
        ));
        assert!(matches!(
            events[2],
            RunEventHistoryEntry::Undecodable(ref event) if event.event_type == "run_finished"
        ));
        assert!(matches!(
            events[3],
            RunEventHistoryEntry::Decoded(ref record)
                if record.event == RunEvent::CycleStarted { cycle_number: 2 }
        ));
    }

    #[tokio::test]
    async fn unrecognized_event_type_column_is_an_unknown_event_type_undecodable_entry() {
        let dir = TempDir::new().unwrap();
        let (db_path, write_pool) = seed_db(dir.path()).await;
        sqlx::query(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("event-future-kind")
        .bind("run-1")
        .bind("workflow_step_added")
        .bind(r#"{"kind":"workflow_step_added","step":"techlead"}"#)
        .bind("2026-07-12T00:00:00+00:00")
        .execute(&write_pool)
        .await
        .unwrap();
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let events = list_events_for_run(&pool, "run-1", ProgressReplay::Included)
            .await
            .expect("an unrecognized event_type must never fail the whole query");

        assert_eq!(events.len(), 1);
        match &events[0] {
            RunEventHistoryEntry::Undecodable(event) => {
                assert_eq!(event.id, "event-future-kind");
                assert_eq!(event.event_type, "workflow_step_added");
                assert_eq!(event.reason, UndecodableReason::UnknownEventType);
            }
            RunEventHistoryEntry::Decoded(record) => {
                panic!("expected an Undecodable entry, got a decoded record: {record:?}")
            }
        }
    }

    #[tokio::test]
    async fn pre_issue_26_untrusted_agent_definition_used_payload_missing_canonical_path_is_undecodable(
    ) {
        let dir = TempDir::new().unwrap();
        let (db_path, write_pool) = seed_db(dir.path()).await;
        let pre_issue_26_payload = r#"{"kind":"untrusted_agent_definition_used","role":"reviewer","path":"/repo/.warden/agents/reviewer.md"}"#;
        sqlx::query(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("event-pre-26")
        .bind("run-1")
        .bind("untrusted_agent_definition_used")
        .bind(pre_issue_26_payload)
        .bind("2026-07-12T00:00:00+00:00")
        .execute(&write_pool)
        .await
        .unwrap();
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let events = list_events_for_run(&pool, "run-1", ProgressReplay::Included)
            .await
            .expect("a stale pre-issue-26 payload must never fail the whole query");

        assert_eq!(events.len(), 1);
        match &events[0] {
            RunEventHistoryEntry::Undecodable(event) => {
                assert_eq!(event.id, "event-pre-26");
                assert_eq!(event.event_type, "untrusted_agent_definition_used");
                assert_eq!(event.reason, UndecodableReason::PayloadDeserialize);
            }
            RunEventHistoryEntry::Decoded(record) => {
                panic!("expected an Undecodable entry, got a decoded record: {record:?}")
            }
        }
    }

    #[tokio::test]
    async fn run_started_payload_with_max_cycles_is_decoded() {
        let dir = TempDir::new().unwrap();
        let (db_path, write_pool) = seed_db(dir.path()).await;
        let pre_issue_43_payload =
            r#"{"kind":"run_started","intent":"do the thing","branch":"main","max_cycles":5}"#;
        sqlx::query(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("event-pre-43")
        .bind("run-1")
        .bind("run_started")
        .bind(pre_issue_43_payload)
        .bind("2026-07-12T00:00:00+00:00")
        .execute(&write_pool)
        .await
        .unwrap();
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let events = list_events_for_run(&pool, "run-1", ProgressReplay::Included)
            .await
            .expect("max_cycles payload must decode");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            RunEventHistoryEntry::Decoded(record)
                if record.event == RunEvent::RunStarted {
                    intent: "do the thing".to_string(),
                    branch: "main".to_string(),
                    max_cycles: 5,
                }
        ));
    }

    #[tokio::test]
    async fn history_where_every_row_is_undecodable_returns_all_of_them() {
        let dir = TempDir::new().unwrap();
        let (db_path, write_pool) = seed_db(dir.path()).await;

        let insert = |id: &'static str,
                      event_type: &'static str,
                      payload: &'static str,
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
        insert(
            "event-1-unknown-kind",
            "workflow_step_added",
            "{}",
            "2026-07-12T00:00:00+00:00",
        )
        .await;
        insert(
            "event-2-malformed",
            "cycle_started",
            "{ not json",
            "2026-07-12T00:00:01+00:00",
        )
        .await;
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let events = list_events_for_run(&pool, "run-1", ProgressReplay::Included)
            .await
            .expect("an all-undecodable history must never fail the whole query");

        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            events
                .iter()
                .all(|entry| matches!(entry, RunEventHistoryEntry::Undecodable(_))),
            "{events:?}"
        );
        let ids: Vec<&str> = events.iter().map(|e| e.id()).collect();
        assert_eq!(ids, vec!["event-1-unknown-kind", "event-2-malformed"]);
    }

    /// Issue #108: the tie-break used to be `id ASC` -- deterministic, but arbitrary with respect
    /// to publication order, since a real `id` is a UUID v4. `rowid ASC` breaks the same tie in
    /// `warden`'s own insertion order, which for this append-only table *is* publication order.
    /// `event-b` is written first on purpose: an `id ASC` fallback would invert the two.
    #[tokio::test]
    async fn rows_sharing_the_same_created_at_replay_in_insertion_order_not_id_order() {
        let dir = TempDir::new().unwrap();
        let (db_path, write_pool) = seed_db(dir.path()).await;
        let same_timestamp = "2026-07-12T00:00:00+00:00";

        sqlx::query(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("event-b")
        .bind("run-1")
        .bind("cycle_started")
        .bind(serde_json::to_string(&RunEvent::CycleStarted { cycle_number: 1 }).unwrap())
        .bind(same_timestamp)
        .execute(&write_pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("event-a")
        .bind("run-1")
        .bind("cycle_started")
        .bind("{ not json")
        .bind(same_timestamp)
        .execute(&write_pool)
        .await
        .unwrap();
        write_pool.close().await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let events = list_events_for_run(&pool, "run-1", ProgressReplay::Included)
            .await
            .unwrap();
        let ids: Vec<&str> = events.iter().map(|e| e.id()).collect();
        assert_eq!(
            ids,
            vec!["event-b", "event-a"],
            "a tied created_at must fall back to insertion order deterministically"
        );
        assert!(matches!(events[0], RunEventHistoryEntry::Decoded(_)));
        assert!(matches!(events[1], RunEventHistoryEntry::Undecodable(_)));
    }
}
