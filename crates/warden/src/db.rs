//! SQLite persistence (ADR-0004). `warden` is the only writer; schema
//! covers `runs`, `cycles`, `findings`, `agent_processes` for this phase
//! (`events`/`evidence` land in later phases). Every row read back is
//! reparsed into a strongly-typed Rust value before leaving this module —
//! callers never see raw strings for `state`/`role`/`source`/`severity`.

use std::path::Path;

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use warden_core::{AgentRole, Finding, FindingSource, RunState, Severity};

use crate::error::{Result, WardenError};

/// Opens (creating if needed) the SQLite database at `db_path`, enables WAL
/// mode so `warden-tui`/`warden-gated` can read concurrently (see
/// code-standards.md, "SQLite & sqlx"), and applies pending migrations.
pub async fn connect(db_path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);

    let pool = SqlitePoolOptions::new().connect_with(options).await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(pool)
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

/// A `runs` row, with `state` already validated into [`RunState`].
#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub repo_path: String,
    pub branch: String,
    pub intent: String,
    pub state: RunState,
    pub max_cycles: u32,
    pub current_cycle: u32,
    pub created_at: String,
    pub updated_at: String,
}

pub async fn insert_run(
    pool: &SqlitePool,
    id: &str,
    repo_path: &str,
    branch: &str,
    intent: &str,
    max_cycles: u32,
) -> Result<()> {
    let now = now_rfc3339();
    let state = RunState::Pending.as_str();
    let max_cycles = i64::from(max_cycles);
    sqlx::query!(
        r#"
        INSERT INTO runs (id, repo_path, branch, intent, state, max_cycles, current_cycle, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?)
        "#,
        id,
        repo_path,
        branch,
        intent,
        state,
        max_cycles,
        now,
        now,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Writes a new state for `run_id`. Callers must call this *before*
/// triggering the corresponding action (write-ahead of intention,
/// ADR-0004) — this function itself does not validate the transition
/// against [`RunState::validate_transition`]; that's the orchestrator's
/// responsibility so the intent is recorded even if the action that
/// follows fails.
pub async fn update_run_state(pool: &SqlitePool, run_id: &str, new_state: RunState) -> Result<()> {
    let now = now_rfc3339();
    let state = new_state.as_str();
    sqlx::query!(
        "UPDATE runs SET state = ?, updated_at = ? WHERE id = ?",
        state,
        now,
        run_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_run_current_cycle(
    pool: &SqlitePool,
    run_id: &str,
    cycle_number: u32,
) -> Result<()> {
    let now = now_rfc3339();
    let cycle_number = i64::from(cycle_number);
    sqlx::query!(
        "UPDATE runs SET current_cycle = ?, updated_at = ? WHERE id = ?",
        cycle_number,
        now,
        run_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Raw shape of a `runs` row as decoded by sqlx, before `state` has been
/// validated into a [`RunState`]. Kept private: [`Run`] is the only form
/// that ever leaves this module.
struct RunRow {
    id: String,
    repo_path: String,
    branch: String,
    intent: String,
    state: String,
    max_cycles: i64,
    current_cycle: i64,
    created_at: String,
    updated_at: String,
}

fn row_to_run(row: RunRow) -> Result<Run> {
    Ok(Run {
        id: row.id,
        repo_path: row.repo_path,
        branch: row.branch,
        intent: row.intent,
        state: RunState::parse(&row.state)?,
        max_cycles: row.max_cycles.try_into().unwrap_or(0),
        current_cycle: row.current_cycle.try_into().unwrap_or(0),
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

pub async fn get_run(pool: &SqlitePool, run_id: &str) -> Result<Option<Run>> {
    let row = sqlx::query_as!(
        RunRow,
        r#"SELECT id as "id!", repo_path, branch, intent, state, max_cycles, current_cycle, created_at, updated_at FROM runs WHERE id = ?"#,
        run_id,
    )
    .fetch_optional(pool)
    .await?;

    row.map(row_to_run).transpose()
}

/// Runs left in an intermediate state (`RunState::is_intermediate`) as of
/// the last shutdown/crash. The three literal state strings below must stay
/// in sync with [`RunState::is_intermediate`] — enforced by a test in this
/// module, since a `?`-parameterised `IN (...)` list isn't expressible in a
/// macro-checked static query.
pub async fn list_intermediate_runs(pool: &SqlitePool) -> Result<Vec<Run>> {
    let rows = sqlx::query_as!(
        RunRow,
        r#"
        SELECT id as "id!", repo_path, branch, intent, state, max_cycles, current_cycle, created_at, updated_at
        FROM runs
        WHERE state IN ('coder_running', 'awaiting_review_test', 'awaiting_ci')
        "#
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(row_to_run).collect()
}

pub async fn insert_cycle(
    pool: &SqlitePool,
    id: &str,
    run_id: &str,
    cycle_number: u32,
) -> Result<()> {
    let now = now_rfc3339();
    let cycle_number = i64::from(cycle_number);
    sqlx::query!(
        "INSERT INTO cycles (id, run_id, cycle_number, started_at) VALUES (?, ?, ?, ?)",
        id,
        run_id,
        cycle_number,
        now,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_cycle_worktree_path(
    pool: &SqlitePool,
    cycle_id: &str,
    role: AgentRole,
    path: &str,
) -> Result<()> {
    match role {
        AgentRole::Coder => {
            sqlx::query!(
                "UPDATE cycles SET coder_worktree_path = ? WHERE id = ?",
                path,
                cycle_id,
            )
            .execute(pool)
            .await?;
        }
        AgentRole::Reviewer => {
            sqlx::query!(
                "UPDATE cycles SET reviewer_worktree_path = ? WHERE id = ?",
                path,
                cycle_id,
            )
            .execute(pool)
            .await?;
        }
        AgentRole::Tester => {
            sqlx::query!(
                "UPDATE cycles SET tester_worktree_path = ? WHERE id = ?",
                path,
                cycle_id,
            )
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

pub async fn close_cycle(pool: &SqlitePool, cycle_id: &str) -> Result<()> {
    let now = now_rfc3339();
    sqlx::query!("UPDATE cycles SET ended_at = ? WHERE id = ?", now, cycle_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn insert_finding(
    pool: &SqlitePool,
    id: &str,
    cycle_id: &str,
    finding: &Finding,
) -> Result<()> {
    let source = finding.source.as_str();
    let severity = finding.severity.as_str();
    sqlx::query!(
        "INSERT INTO findings (id, cycle_id, source, severity, file, description, action) VALUES (?, ?, ?, ?, ?, ?, ?)",
        id,
        cycle_id,
        source,
        severity,
        finding.file,
        finding.description,
        finding.action,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_findings_for_cycle(pool: &SqlitePool, cycle_id: &str) -> Result<Vec<Finding>> {
    let rows = sqlx::query!(
        "SELECT source, severity, file, description, action FROM findings WHERE cycle_id = ?",
        cycle_id,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(Finding {
                source: FindingSource::parse(&r.source)?,
                severity: Severity::parse(&r.severity)?,
                file: r.file,
                description: r.description,
                action: r.action,
            })
        })
        .collect::<std::result::Result<Vec<_>, WardenError>>()
}

pub async fn insert_agent_process(
    pool: &SqlitePool,
    id: &str,
    cycle_id: &str,
    role: AgentRole,
    pid: u32,
    worktree_path: &str,
) -> Result<()> {
    let now = now_rfc3339();
    let role = role.as_str();
    let pid = i64::from(pid);
    sqlx::query!(
        "INSERT INTO agent_processes (id, cycle_id, role, pid, worktree_path, started_at) VALUES (?, ?, ?, ?, ?, ?)",
        id,
        cycle_id,
        role,
        pid,
        worktree_path,
        now,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_agent_process_ended(pool: &SqlitePool, id: &str, exit_code: i32) -> Result<()> {
    let now = now_rfc3339();
    let exit_code = i64::from(exit_code);
    sqlx::query!(
        "UPDATE agent_processes SET ended_at = ?, exit_code = ? WHERE id = ?",
        now,
        exit_code,
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The most recent agent process associated with `run_id` that was never
/// marked as ended — i.e. the process the orchestrator was waiting on when
/// it last wrote to the database. Used by crash recovery: if this process's
/// PID is no longer alive, the run is stuck and must be marked `Failed`.
pub struct OpenAgentProcess {
    pub id: String,
    pub pid: u32,
}

pub async fn latest_open_agent_process_for_run(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Option<OpenAgentProcess>> {
    let row = sqlx::query!(
        r#"
        SELECT agent_processes.id as "id!", agent_processes.pid as "pid!"
        FROM agent_processes
        JOIN cycles ON cycles.id = agent_processes.cycle_id
        WHERE cycles.run_id = ? AND agent_processes.ended_at IS NULL
        ORDER BY agent_processes.started_at DESC
        LIMIT 1
        "#,
        run_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| OpenAgentProcess {
        id: r.id,
        pid: r.pid as u32,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use warden_core::AgentRole;

    async fn test_pool() -> (TempDir, SqlitePool) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let pool = connect(&db_path).await.unwrap();
        (dir, pool)
    }

    #[test]
    fn intermediate_state_literals_match_run_state_is_intermediate() {
        for state in [
            RunState::Pending,
            RunState::CoderRunning,
            RunState::AwaitingReviewTest,
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
            RunState::Done,
            RunState::MaxCyclesExceeded,
            RunState::Failed,
        ] {
            let literal_says_intermediate =
                ["coder_running", "awaiting_review_test", "awaiting_ci"].contains(&state.as_str());
            assert_eq!(
                literal_says_intermediate,
                state.is_intermediate(),
                "state {state:?} disagrees between list_intermediate_runs' literals and RunState::is_intermediate",
            );
        }
    }

    #[tokio::test]
    async fn run_round_trips_through_insert_and_get() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-1", "/tmp/repo", "main", "do the thing", 5)
            .await
            .unwrap();

        let run = get_run(&pool, "run-1").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Pending);
        assert_eq!(run.max_cycles, 5);
        assert_eq!(run.current_cycle, 0);
        assert_eq!(run.intent, "do the thing");
    }

    #[tokio::test]
    async fn update_run_state_persists_and_list_intermediate_finds_it() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-2", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();

        update_run_state(&pool, "run-2", RunState::CoderRunning)
            .await
            .unwrap();

        let run = get_run(&pool, "run-2").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::CoderRunning);

        let intermediate = list_intermediate_runs(&pool).await.unwrap();
        assert_eq!(intermediate.len(), 1);
        assert_eq!(intermediate[0].id, "run-2");
    }

    #[tokio::test]
    async fn converged_run_is_not_listed_as_intermediate() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-3", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        update_run_state(&pool, "run-3", RunState::CoderRunning)
            .await
            .unwrap();
        update_run_state(&pool, "run-3", RunState::AwaitingReviewTest)
            .await
            .unwrap();
        update_run_state(&pool, "run-3", RunState::Converged)
            .await
            .unwrap();

        let intermediate = list_intermediate_runs(&pool).await.unwrap();
        assert!(intermediate.is_empty());
    }

    #[tokio::test]
    async fn cycle_and_finding_round_trip() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-4", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        insert_cycle(&pool, "cycle-1", "run-4", 1).await.unwrap();
        set_cycle_worktree_path(&pool, "cycle-1", AgentRole::Coder, "/tmp/wt/coder")
            .await
            .unwrap();

        let finding = Finding {
            source: FindingSource::Reviewer,
            severity: Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "missing test".to_string(),
            action: Some("add one".to_string()),
        };
        insert_finding(&pool, "finding-1", "cycle-1", &finding)
            .await
            .unwrap();

        let findings = list_findings_for_cycle(&pool, "cycle-1").await.unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0], finding);

        close_cycle(&pool, "cycle-1").await.unwrap();
    }

    #[tokio::test]
    async fn open_agent_process_is_found_until_marked_ended() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-5", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        insert_cycle(&pool, "cycle-5", "run-5", 1).await.unwrap();
        insert_agent_process(
            &pool,
            "proc-1",
            "cycle-5",
            AgentRole::Coder,
            424242,
            "/tmp/wt/coder",
        )
        .await
        .unwrap();

        let open = latest_open_agent_process_for_run(&pool, "run-5")
            .await
            .unwrap();
        assert!(open.is_some());
        assert_eq!(open.unwrap().pid, 424242);

        mark_agent_process_ended(&pool, "proc-1", 0).await.unwrap();

        let open = latest_open_agent_process_for_run(&pool, "run-5")
            .await
            .unwrap();
        assert!(open.is_none());
    }
}
