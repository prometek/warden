//! SQLite persistence (ADR-0004). `warden` is the only writer; schema
//! covers `runs`, `cycles`, `findings`, `agent_processes` for this phase
//! (`events`/`evidence` land in later phases). Every row read back is
//! reparsed into a strongly-typed Rust value before leaving this module —
//! callers never see raw strings for `state`/`role`/`source`/`severity`.

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use warden_core::{AgentRole, Finding, FindingSource, RunState, Severity};

use crate::error::{Result, WardenError};

/// How long a connection waits on SQLite's own lock before giving up with
/// `SQLITE_BUSY`. Matches sqlx's own default (5s) -- named and set
/// explicitly rather than left implicit, because Phase 2 makes concurrent
/// writers a real, expected case (reviewer and tester findings/worktree-path
/// updates land on the same `cycles`/`agent_processes` rows via
/// `tokio::join!`, see orchestrator.rs), not just a theoretical one.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The compiled-in migration set, named so both `connect` (to run it) and
/// `migrations_pending` (to compare against what's already applied) share
/// the exact same source of truth for "how many migrations exist".
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Opens (creating if needed) the SQLite database at `db_path`, enables WAL
/// mode so `warden-tui`/`warden-gated` can read concurrently (see
/// code-standards.md, "SQLite & sqlx"), backs up the database file if
/// pending migrations are about to run against a pre-existing db (issue #6:
/// crash resilience also covers a botched schema migration, not just a
/// crashed run), and applies those migrations.
pub async fn connect(db_path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Captured *before* `connect_with` below, which creates the file if it's
    // missing (`create_if_missing(true)`) — otherwise a brand-new db would
    // always look "pre-existing" by the time we check.
    let db_existed_before_connect = tokio::fs::try_exists(db_path).await?;

    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        // Explicit rather than relying on sqlx's default: the `cycles`,
        // `findings`, and `agent_processes` tables all declare `REFERENCES`
        // clauses (see migrations/0001_initial.sql) that are otherwise
        // decorative — SQLite does not enforce foreign keys unless this
        // pragma is on for the connection.
        .foreign_keys(true)
        // Explicit rather than relying on sqlx's default, for the same
        // reason as `foreign_keys` above: with reviewer and tester now
        // writing concurrently (ADR-0003), a `SQLITE_BUSY` under real WAL
        // write contention is a case worth naming and reasoning about, not
        // an implicit library default.
        .busy_timeout(BUSY_TIMEOUT);

    let pool = SqlitePoolOptions::new().connect_with(options).await?;

    if db_existed_before_connect {
        backup_before_migration(db_path, &pool).await?;
    }

    MIGRATOR.run(&pool).await?;

    Ok(pool)
}

/// `true` if applying [`MIGRATOR`] against `pool` would actually run at
/// least one migration. Deliberately conservative rather than bit-for-bit
/// reproducing `Migrator::run`'s own bookkeeping (dirty-version checks,
/// checksum validation, ...): this only needs to answer "is a backup worth
/// taking", not "is the migration state valid" — `MIGRATOR.run` still does
/// the real validation right after.
async fn migrations_pending(pool: &SqlitePool) -> Result<bool> {
    let migrations_table_exists: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?;

    let Some(_) = migrations_table_exists else {
        // No migrations have ever been recorded against this db file, so
        // every migration `MIGRATOR` knows about is pending (unless there
        // simply aren't any, e.g. a from-scratch schema with no migrations
        // directory — not our case, but kept correct regardless).
        return Ok(MIGRATOR.iter().next().is_some());
    };

    let (applied_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await?;
    let total_migrations = MIGRATOR.iter().count() as i64;

    Ok(applied_count < total_migrations)
}

/// Copies `db_path` to a timestamped sibling (`state.db.bak-<rfc3339>`)
/// before [`MIGRATOR`] is allowed to touch the schema, but only when a
/// migration is actually about to run (see [`migrations_pending`]) — a
/// fresh db or one already on the current schema has nothing worth backing
/// up.
///
/// Uses `VACUUM INTO` rather than a plain filesystem copy of `db_path`: WAL
/// mode (enabled in [`connect`]) means recently committed writes can live
/// only in the `-wal` sidecar file, not yet checkpointed into `db_path`
/// itself, so a bare `fs::copy` could silently produce a backup missing
/// committed data. `VACUUM INTO` reads the database's current *logical*
/// content (WAL included) and materializes it into a single new, consistent
/// file in one step — no separate checkpoint call needed.
///
/// A failure here aborts the migration (propagated to the caller as
/// [`WardenError::Backup`]) rather than proceeding without a safety net
/// (code-standards.md: "no silent fallback").
async fn backup_before_migration(db_path: &Path, pool: &SqlitePool) -> Result<()> {
    if !migrations_pending(pool).await? {
        return Ok(());
    }

    let file_name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.db");
    // `:` is valid in Unix filenames but awkward to work with on the
    // command line, so it's stripped from the timestamp purely for
    // readability — RFC3339 ordering is preserved either way.
    let timestamp = now_rfc3339().replace(':', "-");
    let backup_path = db_path.with_file_name(format!("{file_name}.bak-{timestamp}"));

    sqlx::query("VACUUM INTO ?")
        .bind(backup_path.display().to_string())
        .execute(pool)
        .await
        .map_err(|source| WardenError::Backup {
            path: backup_path.clone(),
            source,
        })?;

    tracing::info!(
        backup_path = %backup_path.display(),
        "backed up SQLite database before applying pending migrations"
    );
    Ok(())
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

/// Converts a `INTEGER` column value into a `u32`, returning a typed error
/// instead of silently clamping/defaulting on overflow (code-standards.md:
/// "no silent fallback"). Every row written by this module comes from a
/// `u32` in the first place, so failure here means the stored value was
/// corrupted or written by something other than this code — worth
/// surfacing, not hiding.
fn checked_u32(value: i64, column: &'static str) -> Result<u32> {
    u32::try_from(value).map_err(|_| WardenError::InvalidStoredValue { column, value })
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
    /// The commit SHA the run converged on (see `set_run_converged_commit`,
    /// M4) — `None` until the run reaches `RunState::Converged`.
    pub converged_commit_sha: Option<String>,
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

/// Records the commit SHA a run converged on (M4). Called once, when the
/// run transitions to `RunState::Converged` — Phase 3's git gate reads this
/// column to know what to push, without needing the (by then removed)
/// coder worktree.
pub async fn set_run_converged_commit(
    pool: &SqlitePool,
    run_id: &str,
    commit_sha: &str,
) -> Result<()> {
    let now = now_rfc3339();
    sqlx::query!(
        "UPDATE runs SET converged_commit_sha = ?, updated_at = ? WHERE id = ?",
        commit_sha,
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
    converged_commit_sha: Option<String>,
}

fn row_to_run(row: RunRow) -> Result<Run> {
    Ok(Run {
        id: row.id,
        repo_path: row.repo_path,
        branch: row.branch,
        intent: row.intent,
        state: RunState::parse(&row.state)?,
        max_cycles: checked_u32(row.max_cycles, "runs.max_cycles")?,
        current_cycle: checked_u32(row.current_cycle, "runs.current_cycle")?,
        created_at: row.created_at,
        updated_at: row.updated_at,
        converged_commit_sha: row.converged_commit_sha,
    })
}

pub async fn get_run(pool: &SqlitePool, run_id: &str) -> Result<Option<Run>> {
    let row = sqlx::query_as!(
        RunRow,
        r#"SELECT id as "id!", repo_path, branch, intent, state, max_cycles, current_cycle, created_at, updated_at, converged_commit_sha FROM runs WHERE id = ?"#,
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
        SELECT id as "id!", repo_path, branch, intent, state, max_cycles, current_cycle, created_at, updated_at, converged_commit_sha
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

/// Records the commit SHA the coder produced during this cycle (M4). Called
/// right after the orchestrator reads the coder worktree's HEAD, so the SHA
/// stays discoverable even after that worktree is removed.
pub async fn set_cycle_commit_sha(
    pool: &SqlitePool,
    cycle_id: &str,
    commit_sha: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE cycles SET coder_commit_sha = ? WHERE id = ?",
        commit_sha,
        cycle_id,
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

/// The distinct, non-null worktree paths recorded across every cycle of
/// `run_id` (`cycles.coder_worktree_path` / `reviewer_worktree_path` /
/// `tester_worktree_path`). Used by crash recovery to find worktrees that
/// may have been orphaned when the orchestrator that owned them died before
/// it could call `Worktree::remove` (issue #6).
pub async fn list_worktree_paths_for_run(pool: &SqlitePool, run_id: &str) -> Result<Vec<String>> {
    let rows = sqlx::query!(
        r#"
        SELECT coder_worktree_path, reviewer_worktree_path, tester_worktree_path
        FROM cycles
        WHERE run_id = ?
        "#,
        run_id,
    )
    .fetch_all(pool)
    .await?;

    let mut paths: Vec<String> = rows
        .into_iter()
        .flat_map(|row| {
            [
                row.coder_worktree_path,
                row.reviewer_worktree_path,
                row.tester_worktree_path,
            ]
        })
        .flatten()
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
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

/// Persists an agent process record, capturing the OS-reported start time
/// of `pid` *at insert time* (H1: PID-reuse hardening). This is what lets
/// `recover_crashed_runs` later tell this exact process instance apart from
/// an unrelated process that reuses the same PID after a reboot — see
/// `process::is_process_alive`. The caller doesn't supply the start time
/// directly: it's derived here, right when the PID is freshest, so callers
/// can't accidentally pass a stale or fabricated value.
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
    let pid_started_at_unix =
        crate::process::process_start_time(pid).unwrap_or(crate::process::UNKNOWN_START_TIME);
    let pid = i64::from(pid);
    sqlx::query!(
        "INSERT INTO agent_processes (id, cycle_id, role, pid, pid_started_at_unix, worktree_path, started_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        id,
        cycle_id,
        role,
        pid,
        pid_started_at_unix,
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
/// PID is no longer alive (or has been reused by an unrelated process, per
/// `pid_started_at_unix`), the run is stuck and must be marked `Failed`.
pub struct OpenAgentProcess {
    pub id: String,
    pub pid: u32,
    pub pid_started_at_unix: i64,
}

pub async fn latest_open_agent_process_for_run(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Option<OpenAgentProcess>> {
    let row = sqlx::query!(
        r#"
        SELECT agent_processes.id as "id!", agent_processes.pid as "pid!", agent_processes.pid_started_at_unix as "pid_started_at_unix!"
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

    row.map(|r| {
        Ok(OpenAgentProcess {
            id: r.id,
            pid: checked_u32(r.pid, "agent_processes.pid")?,
            pid_started_at_unix: r.pid_started_at_unix,
        })
    })
    .transpose()
}

/// Every agent process associated with `run_id` that was never marked
/// ended, not just the most recent one (as [`latest_open_agent_process_for_run`]
/// returns, used only to decide whether a run is still legitimately in
/// progress). Reviewer and tester run concurrently (ADR-0003), so more than
/// one row can be open at once — crash recovery needs all of them to
/// terminate every orphaned process, not just the newest.
pub async fn list_open_agent_processes_for_run(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Vec<OpenAgentProcess>> {
    let rows = sqlx::query!(
        r#"
        SELECT agent_processes.id as "id!", agent_processes.pid as "pid!", agent_processes.pid_started_at_unix as "pid_started_at_unix!"
        FROM agent_processes
        JOIN cycles ON cycles.id = agent_processes.cycle_id
        WHERE cycles.run_id = ? AND agent_processes.ended_at IS NULL
        ORDER BY agent_processes.started_at DESC
        "#,
        run_id,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(OpenAgentProcess {
                id: r.id,
                pid: checked_u32(r.pid, "agent_processes.pid")?,
                pid_started_at_unix: r.pid_started_at_unix,
            })
        })
        .collect()
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
    async fn get_run_returns_none_for_an_unknown_id() {
        let (_dir, pool) = test_pool().await;
        let run = get_run(&pool, "does-not-exist").await.unwrap();
        assert!(run.is_none());
    }

    #[tokio::test]
    async fn inserting_a_run_with_a_duplicate_id_is_a_typed_error_not_a_panic() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "dup-run", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();

        let result = insert_run(&pool, "dup-run", "/tmp/repo", "main", "intent again", 3).await;
        assert!(matches!(result, Err(WardenError::Database(_))));

        // The original row must be untouched by the failed duplicate insert.
        let run = get_run(&pool, "dup-run").await.unwrap().unwrap();
        assert_eq!(run.intent, "intent");
    }

    #[tokio::test]
    async fn list_findings_for_cycle_with_no_findings_is_empty_not_an_error() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-empty", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        insert_cycle(&pool, "cycle-empty", "run-empty", 1)
            .await
            .unwrap();

        let findings = list_findings_for_cycle(&pool, "cycle-empty").await.unwrap();
        assert!(findings.is_empty());
    }

    #[tokio::test]
    async fn latest_open_agent_process_is_none_when_run_has_no_processes() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-no-proc", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();

        let open = latest_open_agent_process_for_run(&pool, "run-no-proc")
            .await
            .unwrap();
        assert!(open.is_none());
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

    #[tokio::test]
    async fn list_open_agent_processes_returns_every_open_row_not_just_the_latest() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-6", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        insert_cycle(&pool, "cycle-6", "run-6", 1).await.unwrap();

        // Reviewer and tester open concurrently (ADR-0003): both rows must
        // come back, not just whichever sorts last.
        insert_agent_process(
            &pool,
            "proc-reviewer",
            "cycle-6",
            AgentRole::Reviewer,
            111,
            "/tmp/wt/reviewer",
        )
        .await
        .unwrap();
        insert_agent_process(
            &pool,
            "proc-tester",
            "cycle-6",
            AgentRole::Tester,
            222,
            "/tmp/wt/tester",
        )
        .await
        .unwrap();
        // Already closed: must not be returned.
        insert_agent_process(
            &pool,
            "proc-coder",
            "cycle-6",
            AgentRole::Coder,
            333,
            "/tmp/wt/coder",
        )
        .await
        .unwrap();
        mark_agent_process_ended(&pool, "proc-coder", 0)
            .await
            .unwrap();

        let mut open = list_open_agent_processes_for_run(&pool, "run-6")
            .await
            .unwrap();
        open.sort_by_key(|p| p.pid);
        let pids: Vec<u32> = open.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![111, 222]);
    }

    #[tokio::test]
    async fn list_open_agent_processes_is_empty_for_a_run_with_no_processes() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-7", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();

        let open = list_open_agent_processes_for_run(&pool, "run-7")
            .await
            .unwrap();
        assert!(open.is_empty());
    }

    #[tokio::test]
    async fn list_worktree_paths_collects_distinct_non_null_paths_across_cycles() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-8", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        insert_cycle(&pool, "cycle-8a", "run-8", 1).await.unwrap();
        insert_cycle(&pool, "cycle-8b", "run-8", 2).await.unwrap();

        set_cycle_worktree_path(&pool, "cycle-8a", AgentRole::Coder, "/tmp/wt/coder-1")
            .await
            .unwrap();
        set_cycle_worktree_path(&pool, "cycle-8a", AgentRole::Reviewer, "/tmp/wt/reviewer-1")
            .await
            .unwrap();
        set_cycle_worktree_path(&pool, "cycle-8b", AgentRole::Coder, "/tmp/wt/coder-2")
            .await
            .unwrap();
        // Tester path left unset for both cycles — must not appear as a
        // spurious empty/None entry.

        let mut paths = list_worktree_paths_for_run(&pool, "run-8").await.unwrap();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "/tmp/wt/coder-1".to_string(),
                "/tmp/wt/coder-2".to_string(),
                "/tmp/wt/reviewer-1".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn list_worktree_paths_is_empty_for_a_run_with_no_cycles() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-9", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();

        let paths = list_worktree_paths_for_run(&pool, "run-9").await.unwrap();
        assert!(paths.is_empty());
    }

    #[tokio::test]
    async fn connect_does_not_back_up_a_brand_new_database_file() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");

        connect(&db_path).await.unwrap();

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bak-"))
            .collect();
        assert!(
            backups.is_empty(),
            "a freshly created db must not be backed up: {backups:?}"
        );
    }

    #[tokio::test]
    async fn connect_does_not_back_up_when_the_schema_is_already_current() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");

        // First connect creates the file and applies every migration.
        connect(&db_path).await.unwrap();
        // Second connect against the same file: schema is already current,
        // so no migration is about to run — nothing worth backing up.
        connect(&db_path).await.unwrap();

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bak-"))
            .collect();
        assert!(
            backups.is_empty(),
            "reconnecting to an up-to-date schema must not produce a backup: {backups:?}"
        );
    }

    #[tokio::test]
    async fn connect_backs_up_a_pre_existing_database_before_applying_pending_migrations() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");

        // Simulate an older Warden installation: only the first migration
        // has ever been applied (`Migrator::run_to`, sqlx's own supported
        // way to stop partway through), so the rest are still pending on
        // the next `connect`.
        {
            let options = SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal);
            let pool = SqlitePoolOptions::new()
                .connect_with(options)
                .await
                .unwrap();

            let first_migration_version = MIGRATOR.iter().next().unwrap().version;
            MIGRATOR
                .run_to(first_migration_version, &pool)
                .await
                .unwrap();
            pool.close().await;
        }

        connect(&db_path).await.unwrap();

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bak-"))
            .collect();
        assert_eq!(
            backups.len(),
            1,
            "a pre-existing db with pending migrations must be backed up exactly once: {backups:?}"
        );
    }
}
