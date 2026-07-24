//! SQLite persistence (ADR-0004). `warden` is the only writer; schema
//! covers `runs`, `cycles`, `findings`, `agent_processes`, `evidence`
//! (Phase 7, ADR-0009, issue #7), and (Phase 8, ADR-0008) `events`. Every
//! row read back is reparsed into a strongly-typed Rust value before
//! leaving this module — callers never see raw strings for
//! `state`/`role`/`source`/`severity`/`event_type`.

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use warden_core::{
    AgentRole, EventKind, EvidenceType, Finding, FindingSource, RunEvent, RunEventRecord, RunState,
    Severity, TokenUsage,
};

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

    // `:` is valid in Unix filenames but awkward to work with on the
    // command line, so it's stripped from the timestamp purely for
    // readability — RFC3339 ordering is preserved either way.
    let timestamp = now_rfc3339().replace(':', "-");
    let backup_path = unique_backup_path(db_path, &timestamp).await?;

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

/// Picks a backup path of the form `<file_name>.bak-<timestamp>`, appending
/// `-1`, `-2`, ... if that name is already taken. `now_rfc3339()`'s
/// resolution isn't guaranteed finer than a second on every platform, so two
/// backups requested within the same second (or a stale leftover file from a
/// previous run sharing the same timestamp) must not collide — `VACUUM INTO`
/// refuses to overwrite an existing file, which would otherwise abort the
/// migration on a spurious naming collision rather than a real backup
/// failure.
async fn unique_backup_path(db_path: &Path, timestamp: &str) -> Result<std::path::PathBuf> {
    let file_name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.db");

    let mut candidate = db_path.with_file_name(format!("{file_name}.bak-{timestamp}"));
    let mut suffix: u32 = 1;
    while tokio::fs::try_exists(&candidate).await? {
        candidate = db_path.with_file_name(format!("{file_name}.bak-{timestamp}-{suffix}"));
        suffix += 1;
    }
    Ok(candidate)
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

/// Same contract as [`checked_u32`], for the wider token-count columns
/// (issue #53) -- a single agent invocation's usage comfortably fits `u32`,
/// but a run's accumulated total (`add_run_token_usage`) is not bounded the
/// same way over an arbitrarily long-running convergence loop.
fn checked_u64(value: i64, column: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| WardenError::InvalidStoredValue { column, value })
}

/// Converts a possibly-`NULL` `TokenUsage` column group read back from
/// `cycles`/`runs` (issue #53) into `Option<TokenUsage>` -- `None` only when
/// *every* one of the four columns is `NULL` (no usage was ever recorded for
/// this role/run), never when just the two cache columns are (a tool that
/// reports input/output but never caching is still a real, known usage
/// report, not "n/a" -- see `warden_core::TokenUsage`'s own docs on this
/// distinction).
fn row_to_token_usage(
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
) -> Result<Option<TokenUsage>> {
    if input_tokens.is_none()
        && output_tokens.is_none()
        && cache_read_tokens.is_none()
        && cache_creation_tokens.is_none()
    {
        return Ok(None);
    }
    Ok(Some(TokenUsage::new(
        checked_u64(input_tokens.unwrap_or(0), "input_tokens")?,
        checked_u64(output_tokens.unwrap_or(0), "output_tokens")?,
        cache_read_tokens
            .map(|value| checked_u64(value, "cache_read_tokens"))
            .transpose()?,
        cache_creation_tokens
            .map(|value| checked_u64(value, "cache_creation_tokens"))
            .transpose()?,
    )))
}

/// A `runs` row, with `state` already validated into [`RunState`].
///
/// Issue #43 (#37.4) / ADR-0014: `max_cycles`/`current_cycle` are gone,
/// replaced by two independent per-phase budgets/counters -- see
/// `crates/warden/migrations/0007_phase_budgets.sql`. Issue #73:
/// `total_steps`/`max_extra_step_cycles`/`current_extra_step_cycle` back the
/// generic, step-indexed `warden_core::RunState::RunningStep`/
/// `StepCyclesExceeded` -- see `crates/warden/migrations/0009_generic_workflow_state.sql`.
#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub repo_path: String,
    pub branch: String,
    pub intent: String,
    pub state: RunState,
    pub max_review_cycles: u32,
    pub max_test_cycles: u32,
    pub current_review_cycle: u32,
    pub current_test_cycle: u32,
    /// Issue #73: how many steps this run's own resolved
    /// `warden_core::Workflow` has (`workflow.steps.len()`) -- what
    /// `RunState::validate_transition` needs to decide whether a step is the
    /// workflow's *last* one. `3` for every run driving the built-in default
    /// workflow (coder, reviewer, tester).
    pub total_steps: u32,
    /// Issue #73: the single shared cycle budget for any workflow step
    /// beyond the built-in reviewer/tester pair (e.g. a custom `techlead`
    /// step) -- the built-in pair keeps its own `max_review_cycles`/
    /// `max_test_cycles` above.
    pub max_extra_step_cycles: u32,
    pub current_extra_step_cycle: u32,
    pub created_at: String,
    pub updated_at: String,
    /// The commit SHA the run converged on (see `set_run_converged_commit`,
    /// M4) — `None` until the run reaches `RunState::Converged`.
    pub converged_commit_sha: Option<String>,
    /// The PR `warden-gated` opened for this run (see `set_run_pr_number`,
    /// issue #15/ADR-0011) — `None` until `Pushed`'s tail successfully opens
    /// one. Read back by crash recovery to resume a stuck `AwaitingCi` watch
    /// without needing any watch state of `warden-gated`'s own.
    pub pr_number: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_run(
    pool: &SqlitePool,
    id: &str,
    repo_path: &str,
    branch: &str,
    intent: &str,
    max_review_cycles: u32,
    max_test_cycles: u32,
    total_steps: u32,
    max_extra_step_cycles: u32,
) -> Result<()> {
    let now = now_rfc3339();
    let state = RunState::Pending.as_str();
    let max_review_cycles = i64::from(max_review_cycles);
    let max_test_cycles = i64::from(max_test_cycles);
    let total_steps = i64::from(total_steps);
    let max_extra_step_cycles = i64::from(max_extra_step_cycles);
    sqlx::query!(
        r#"
        INSERT INTO runs (id, repo_path, branch, intent, state, max_review_cycles, max_test_cycles, current_review_cycle, current_test_cycle, total_steps, max_extra_step_cycles, current_extra_step_cycle, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0, ?, ?, 0, ?, ?)
        "#,
        id,
        repo_path,
        branch,
        intent,
        state,
        max_review_cycles,
        max_test_cycles,
        total_steps,
        max_extra_step_cycles,
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

/// Issue #43: records the run's current *review* cycle number -- the
/// reviewer runs every cycle (Phase A gate, issue #41), so this tracks the
/// run's overall cycle number exactly like the old, single `current_cycle`
/// did.
pub async fn set_run_current_review_cycle(
    pool: &SqlitePool,
    run_id: &str,
    review_cycle: u32,
) -> Result<()> {
    let now = now_rfc3339();
    let review_cycle = i64::from(review_cycle);
    sqlx::query!(
        "UPDATE runs SET current_review_cycle = ?, updated_at = ? WHERE id = ?",
        review_cycle,
        now,
        run_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Issue #43: records the run's current *test* cycle number -- unlike
/// review, the tester only actually runs on a cycle whose review came back
/// clean (issue #41's gate), so this only advances then.
pub async fn set_run_current_test_cycle(
    pool: &SqlitePool,
    run_id: &str,
    test_cycle: u32,
) -> Result<()> {
    let now = now_rfc3339();
    let test_cycle = i64::from(test_cycle);
    sqlx::query!(
        "UPDATE runs SET current_test_cycle = ?, updated_at = ? WHERE id = ?",
        test_cycle,
        now,
        run_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Issue #73: records the run's current *extra-step* cycle number -- the
/// single shared counter for any workflow step beyond the built-in
/// reviewer/tester pair (see [`Run`]'s own docs on `max_extra_step_cycles`).
pub async fn set_run_current_extra_step_cycle(
    pool: &SqlitePool,
    run_id: &str,
    extra_step_cycle: u32,
) -> Result<()> {
    let now = now_rfc3339();
    let extra_step_cycle = i64::from(extra_step_cycle);
    sqlx::query!(
        "UPDATE runs SET current_extra_step_cycle = ?, updated_at = ? WHERE id = ?",
        extra_step_cycle,
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

/// Records the PR `warden-gated` opened for this run (issue #15/ADR-0011),
/// once the post-Converged tail's `OpenDraft` succeeds. `warden` is still
/// the sole writer of this column -- `warden-gated` only ever reads it back
/// read-only (`get_run_view`-style query), e.g. to resume a stuck
/// `AwaitingCi` watch after a crash without keeping any watch state itself.
pub async fn set_run_pr_number(pool: &SqlitePool, run_id: &str, pr_number: u64) -> Result<()> {
    let now = now_rfc3339();
    // Issue #15 review, L2: reports the real `u64` value that failed to
    // convert -- `WardenError::InvalidStoredValue` (used elsewhere in this
    // module for the *opposite* direction, i64 column -> smaller unsigned
    // type) can only carry an `i64`, which would have silently misreported
    // an overflowing pr_number as `i64::MAX` instead of its actual value.
    let stored_pr_number =
        i64::try_from(pr_number).map_err(|_| WardenError::PrNumberOverflow { pr_number })?;
    sqlx::query!(
        "UPDATE runs SET pr_number = ?, updated_at = ? WHERE id = ?",
        stored_pr_number,
        now,
        run_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Issue #53: accumulates one agent invocation's token usage onto `run_id`'s
/// running total -- the run-level half of the "per agent / per cycle / run
/// total" aggregation (the cycle-level half is
/// [`add_cycle_role_token_usage`]). Both are always called together, from
/// the same call site (`orchestrator::Orchestrator::run_agent`), right after
/// an invocation's `ToolAdapter::extract_usage` reports `Some`.
///
/// `input_tokens`/`output_tokens` are unconditionally summed
/// (`COALESCE(column, 0) + ?`); the cache columns only advance when `usage`
/// itself reports that dimension (`CASE WHEN ? IS NULL THEN <unchanged>
/// ELSE ...`) -- an invocation that doesn't report caching must never reset
/// a running cache total a prior invocation already built up. See
/// [`row_to_token_usage`] for the read-back side of this same "`NULL` means
/// never-reported, not zero" contract.
pub async fn add_run_token_usage(
    pool: &SqlitePool,
    run_id: &str,
    usage: &TokenUsage,
) -> Result<()> {
    let input_tokens = checked_i64(usage.input_tokens, "runs.total_input_tokens")?;
    let output_tokens = checked_i64(usage.output_tokens, "runs.total_output_tokens")?;
    let cache_read_tokens = usage
        .cache_read_tokens
        .map(|value| checked_i64(value, "runs.total_cache_read_tokens"))
        .transpose()?;
    let cache_creation_tokens = usage
        .cache_creation_tokens
        .map(|value| checked_i64(value, "runs.total_cache_creation_tokens"))
        .transpose()?;
    let now = now_rfc3339();
    sqlx::query!(
        r#"
        UPDATE runs SET
            total_input_tokens = COALESCE(total_input_tokens, 0) + ?,
            total_output_tokens = COALESCE(total_output_tokens, 0) + ?,
            total_cache_read_tokens = CASE WHEN ? IS NULL THEN total_cache_read_tokens ELSE COALESCE(total_cache_read_tokens, 0) + ? END,
            total_cache_creation_tokens = CASE WHEN ? IS NULL THEN total_cache_creation_tokens ELSE COALESCE(total_cache_creation_tokens, 0) + ? END,
            updated_at = ?
        WHERE id = ?
        "#,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        cache_creation_tokens,
        now,
        run_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The run total accumulated so far by [`add_run_token_usage`], or `None` if
/// this run's tool never reported any usage at all (rendered "n/a" by every
/// caller, never `0` -- see [`row_to_token_usage`]).
pub async fn get_run_token_usage(pool: &SqlitePool, run_id: &str) -> Result<Option<TokenUsage>> {
    let row = sqlx::query!(
        r#"
        SELECT total_input_tokens, total_output_tokens, total_cache_read_tokens, total_cache_creation_tokens
        FROM runs WHERE id = ?
        "#,
        run_id,
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    row_to_token_usage(
        row.total_input_tokens,
        row.total_output_tokens,
        row.total_cache_read_tokens,
        row.total_cache_creation_tokens,
    )
}

/// Converts a `u64` [`TokenUsage`] field into the `i64` SQLite's native
/// integer column actually stores -- same contract as `set_run_pr_number`'s
/// own `u64` -> `i64` conversion (issue #15 review, L2): surfaces the real
/// value that overflowed rather than silently truncating/clamping it
/// (code-standards.md: "no silent fallback").
fn checked_i64(value: u64, column: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| WardenError::TokenCountOverflow { column, value })
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
    max_review_cycles: i64,
    max_test_cycles: i64,
    current_review_cycle: i64,
    current_test_cycle: i64,
    total_steps: i64,
    max_extra_step_cycles: i64,
    current_extra_step_cycle: i64,
    created_at: String,
    updated_at: String,
    converged_commit_sha: Option<String>,
    pr_number: Option<i64>,
}

fn row_to_run(row: RunRow) -> Result<Run> {
    let pr_number = row
        .pr_number
        .map(|value| checked_u32(value, "runs.pr_number").map(u64::from))
        .transpose()?;
    Ok(Run {
        id: row.id,
        repo_path: row.repo_path,
        branch: row.branch,
        intent: row.intent,
        state: RunState::parse(&row.state)?,
        max_review_cycles: checked_u32(row.max_review_cycles, "runs.max_review_cycles")?,
        max_test_cycles: checked_u32(row.max_test_cycles, "runs.max_test_cycles")?,
        current_review_cycle: checked_u32(row.current_review_cycle, "runs.current_review_cycle")?,
        current_test_cycle: checked_u32(row.current_test_cycle, "runs.current_test_cycle")?,
        total_steps: checked_u32(row.total_steps, "runs.total_steps")?,
        max_extra_step_cycles: checked_u32(
            row.max_extra_step_cycles,
            "runs.max_extra_step_cycles",
        )?,
        current_extra_step_cycle: checked_u32(
            row.current_extra_step_cycle,
            "runs.current_extra_step_cycle",
        )?,
        created_at: row.created_at,
        updated_at: row.updated_at,
        converged_commit_sha: row.converged_commit_sha,
        pr_number,
    })
}

pub async fn get_run(pool: &SqlitePool, run_id: &str) -> Result<Option<Run>> {
    let row = sqlx::query_as!(
        RunRow,
        r#"SELECT id as "id!", repo_path, branch, intent, state, max_review_cycles, max_test_cycles, current_review_cycle, current_test_cycle, total_steps, max_extra_step_cycles, current_extra_step_cycle, created_at, updated_at, converged_commit_sha, pr_number FROM runs WHERE id = ?"#,
        run_id,
    )
    .fetch_optional(pool)
    .await?;

    row.map(row_to_run).transpose()
}

/// Runs left in an intermediate state (`RunState::is_intermediate`) as of
/// the last shutdown/crash. The `coder_running`/`awaiting_ci` literals and
/// the `running_step:%` pattern below must stay in sync with
/// [`RunState::is_intermediate`] — enforced by a test in this module, since
/// a `?`-parameterised `IN (...)` list isn't expressible in a
/// macro-checked static query. `running_step:%` is a `LIKE` pattern rather
/// than a literal list (issue #73): a step-indexed state can carry any
/// index, so there is no fixed set of literal strings left to enumerate.
pub async fn list_intermediate_runs(pool: &SqlitePool) -> Result<Vec<Run>> {
    let rows = sqlx::query_as!(
        RunRow,
        r#"
        SELECT id as "id!", repo_path, branch, intent, state, max_review_cycles, max_test_cycles, current_review_cycle, current_test_cycle, total_steps, max_extra_step_cycles, current_extra_step_cycle, created_at, updated_at, converged_commit_sha, pr_number
        FROM runs
        WHERE state IN ('coder_running', 'awaiting_ci') OR state LIKE 'running_step:%'
        "#
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(row_to_run).collect()
}

/// `Failed` runs that may still have orphaned resources needing cleanup: an
/// `agent_processes` row never marked ended, or a `cycles` row still
/// recording a worktree path (only cleared once crash recovery successfully
/// removes it — see [`clear_cycle_worktree_path`]).
///
/// This exists because [`list_intermediate_runs`] alone is not enough for
/// crash-safe recovery (issue #6): `recover_crashed_runs` writes `Failed`
/// *before* attempting orphan cleanup (write-ahead of intention, ADR-0004),
/// so if the orchestrator crashes again in the window between that write and
/// cleanup finishing, the run is already `Failed` — no longer
/// `is_intermediate()` — and `list_intermediate_runs` would never surface it
/// again, permanently leaking its worktree/process. A run whose cleanup
/// already succeeded has neither an open process nor a recorded path left,
/// so it naturally stops being returned here — no separate "cleanup done"
/// flag needed.
pub async fn list_failed_runs_with_pending_cleanup(pool: &SqlitePool) -> Result<Vec<Run>> {
    let rows = sqlx::query_as!(
        RunRow,
        r#"
        SELECT DISTINCT runs.id as "id!", runs.repo_path, runs.branch, runs.intent, runs.state, runs.max_review_cycles, runs.max_test_cycles, runs.current_review_cycle, runs.current_test_cycle, runs.total_steps, runs.max_extra_step_cycles, runs.current_extra_step_cycle, runs.created_at, runs.updated_at, runs.converged_commit_sha, runs.pr_number
        FROM runs
        WHERE runs.state = 'failed'
          AND (
            EXISTS (
                SELECT 1 FROM agent_processes
                JOIN cycles ON cycles.id = agent_processes.cycle_id
                WHERE cycles.run_id = runs.id AND agent_processes.ended_at IS NULL
            )
            OR EXISTS (
                SELECT 1 FROM cycles
                WHERE cycles.run_id = runs.id
                  AND (cycles.coder_worktree_path IS NOT NULL
                       OR cycles.reviewer_worktree_path IS NOT NULL
                       OR cycles.tester_worktree_path IS NOT NULL)
            )
          )
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

/// Nulls out the recorded worktree path for `role` on `cycle_id`, once crash
/// recovery has actually removed that worktree from disk (issue #6). This is
/// what lets [`list_failed_runs_with_pending_cleanup`] stop returning a run
/// after its orphan cleanup succeeds — the run stays `Failed` forever (a
/// terminal state), but the *recorded path* is the signal that tells a later
/// recovery pass whether there is still anything left to reclaim for it.
pub async fn clear_cycle_worktree_path(
    pool: &SqlitePool,
    cycle_id: &str,
    role: AgentRole,
) -> Result<()> {
    match role {
        AgentRole::Coder => {
            sqlx::query!(
                "UPDATE cycles SET coder_worktree_path = NULL WHERE id = ?",
                cycle_id,
            )
            .execute(pool)
            .await?;
        }
        AgentRole::Reviewer => {
            sqlx::query!(
                "UPDATE cycles SET reviewer_worktree_path = NULL WHERE id = ?",
                cycle_id,
            )
            .execute(pool)
            .await?;
        }
        AgentRole::Tester => {
            sqlx::query!(
                "UPDATE cycles SET tester_worktree_path = NULL WHERE id = ?",
                cycle_id,
            )
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Issue #53: accumulates one agent invocation's token usage onto `role`'s
/// running total for this cycle -- the cycle-level half of the aggregation
/// (see [`add_run_token_usage`]'s own docs for the run-level half and the
/// shared "cache columns only advance when reported" rule both follow).
/// Three near-identical arms rather than a dynamic column name, matching
/// [`set_cycle_worktree_path`]'s own per-role match (code-standards.md: no
/// SQL built by string concatenation).
pub async fn add_cycle_role_token_usage(
    pool: &SqlitePool,
    cycle_id: &str,
    role: AgentRole,
    usage: &TokenUsage,
) -> Result<()> {
    let input_tokens = checked_i64(usage.input_tokens, "cycles.<role>_input_tokens")?;
    let output_tokens = checked_i64(usage.output_tokens, "cycles.<role>_output_tokens")?;
    let cache_read_tokens = usage
        .cache_read_tokens
        .map(|value| checked_i64(value, "cycles.<role>_cache_read_tokens"))
        .transpose()?;
    let cache_creation_tokens = usage
        .cache_creation_tokens
        .map(|value| checked_i64(value, "cycles.<role>_cache_creation_tokens"))
        .transpose()?;

    match role {
        AgentRole::Coder => {
            sqlx::query!(
                r#"
                UPDATE cycles SET
                    coder_input_tokens = COALESCE(coder_input_tokens, 0) + ?,
                    coder_output_tokens = COALESCE(coder_output_tokens, 0) + ?,
                    coder_cache_read_tokens = CASE WHEN ? IS NULL THEN coder_cache_read_tokens ELSE COALESCE(coder_cache_read_tokens, 0) + ? END,
                    coder_cache_creation_tokens = CASE WHEN ? IS NULL THEN coder_cache_creation_tokens ELSE COALESCE(coder_cache_creation_tokens, 0) + ? END
                WHERE id = ?
                "#,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                cache_creation_tokens,
                cycle_id,
            )
            .execute(pool)
            .await?;
        }
        AgentRole::Reviewer => {
            sqlx::query!(
                r#"
                UPDATE cycles SET
                    reviewer_input_tokens = COALESCE(reviewer_input_tokens, 0) + ?,
                    reviewer_output_tokens = COALESCE(reviewer_output_tokens, 0) + ?,
                    reviewer_cache_read_tokens = CASE WHEN ? IS NULL THEN reviewer_cache_read_tokens ELSE COALESCE(reviewer_cache_read_tokens, 0) + ? END,
                    reviewer_cache_creation_tokens = CASE WHEN ? IS NULL THEN reviewer_cache_creation_tokens ELSE COALESCE(reviewer_cache_creation_tokens, 0) + ? END
                WHERE id = ?
                "#,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                cache_creation_tokens,
                cycle_id,
            )
            .execute(pool)
            .await?;
        }
        AgentRole::Tester => {
            sqlx::query!(
                r#"
                UPDATE cycles SET
                    tester_input_tokens = COALESCE(tester_input_tokens, 0) + ?,
                    tester_output_tokens = COALESCE(tester_output_tokens, 0) + ?,
                    tester_cache_read_tokens = CASE WHEN ? IS NULL THEN tester_cache_read_tokens ELSE COALESCE(tester_cache_read_tokens, 0) + ? END,
                    tester_cache_creation_tokens = CASE WHEN ? IS NULL THEN tester_cache_creation_tokens ELSE COALESCE(tester_cache_creation_tokens, 0) + ? END
                WHERE id = ?
                "#,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                cache_creation_tokens,
                cycle_id,
            )
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Raw shape of `cycles`' twelve per-role token-count columns (issue #53),
/// read back once per [`get_cycle_role_token_usage`] call and then narrowed
/// to the requested `role`'s four columns -- avoids three near-duplicate
/// `SELECT`s (one per role) for what is, from the database's own point of
/// view, a single row read.
struct CycleTokenUsageRow {
    coder_input_tokens: Option<i64>,
    coder_output_tokens: Option<i64>,
    coder_cache_read_tokens: Option<i64>,
    coder_cache_creation_tokens: Option<i64>,
    reviewer_input_tokens: Option<i64>,
    reviewer_output_tokens: Option<i64>,
    reviewer_cache_read_tokens: Option<i64>,
    reviewer_cache_creation_tokens: Option<i64>,
    tester_input_tokens: Option<i64>,
    tester_output_tokens: Option<i64>,
    tester_cache_read_tokens: Option<i64>,
    tester_cache_creation_tokens: Option<i64>,
}

/// The running total accumulated by [`add_cycle_role_token_usage`] for
/// `role` on `cycle_id`, or `None` if that role never reported any usage on
/// this cycle (e.g. it hasn't run yet, or its tool reports no usage at all
/// -- rendered "n/a" by every caller, see [`row_to_token_usage`]).
pub async fn get_cycle_role_token_usage(
    pool: &SqlitePool,
    cycle_id: &str,
    role: AgentRole,
) -> Result<Option<TokenUsage>> {
    let row = sqlx::query_as!(
        CycleTokenUsageRow,
        r#"
        SELECT coder_input_tokens, coder_output_tokens, coder_cache_read_tokens, coder_cache_creation_tokens,
               reviewer_input_tokens, reviewer_output_tokens, reviewer_cache_read_tokens, reviewer_cache_creation_tokens,
               tester_input_tokens, tester_output_tokens, tester_cache_read_tokens, tester_cache_creation_tokens
        FROM cycles WHERE id = ?
        "#,
        cycle_id,
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let (input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens) = match role {
        AgentRole::Coder => (
            row.coder_input_tokens,
            row.coder_output_tokens,
            row.coder_cache_read_tokens,
            row.coder_cache_creation_tokens,
        ),
        AgentRole::Reviewer => (
            row.reviewer_input_tokens,
            row.reviewer_output_tokens,
            row.reviewer_cache_read_tokens,
            row.reviewer_cache_creation_tokens,
        ),
        AgentRole::Tester => (
            row.tester_input_tokens,
            row.tester_output_tokens,
            row.tester_cache_read_tokens,
            row.tester_cache_creation_tokens,
        ),
    };
    row_to_token_usage(
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
    )
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

/// A single recorded worktree path for one cycle/role, together with enough
/// identity (`cycle_id`, `role`) to clear it via
/// [`clear_cycle_worktree_path`] once crash recovery has removed it from
/// disk — unlike [`list_worktree_paths_for_run`], which flattens/dedups
/// paths for simple removal and loses that association.
pub struct CycleWorktreeEntry {
    pub cycle_id: String,
    pub role: AgentRole,
    pub path: String,
}

/// Every non-null worktree path recorded across `run_id`'s cycles, tagged
/// with the cycle/role it came from. Used by crash recovery so a
/// successfully removed worktree's path can be cleared afterwards
/// (issue #6): without that, a `Failed` run would look like it still has
/// orphaned worktrees forever, since the path column is otherwise never
/// cleared once a cycle records it.
pub async fn list_cycle_worktree_entries_for_run(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Vec<CycleWorktreeEntry>> {
    let rows = sqlx::query!(
        r#"
        SELECT id as "id!", coder_worktree_path, reviewer_worktree_path, tester_worktree_path
        FROM cycles
        WHERE run_id = ?
        "#,
        run_id,
    )
    .fetch_all(pool)
    .await?;

    let mut entries = Vec::new();
    for row in rows {
        if let Some(path) = row.coder_worktree_path {
            entries.push(CycleWorktreeEntry {
                cycle_id: row.id.clone(),
                role: AgentRole::Coder,
                path,
            });
        }
        if let Some(path) = row.reviewer_worktree_path {
            entries.push(CycleWorktreeEntry {
                cycle_id: row.id.clone(),
                role: AgentRole::Reviewer,
                path,
            });
        }
        if let Some(path) = row.tester_worktree_path {
            entries.push(CycleWorktreeEntry {
                cycle_id: row.id.clone(),
                role: AgentRole::Tester,
                path,
            });
        }
    }
    Ok(entries)
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

/// LOW (issue #20 review): `ORDER BY id ASC` makes the returned order
/// deterministic -- without it, SQLite is free to return `findings` rows in
/// any order for a given `cycle_id`, which fed straight into
/// `AgentInputMessage::for_finding_agent`'s `findings` field (ADR-0012)
/// would make the reviewer/tester's prior-findings context vary run to run
/// for identical data. `id` (not a timestamp -- `findings` has none) is
/// good enough for determinism; it doesn't need to reflect insertion order.
pub async fn list_findings_for_cycle(pool: &SqlitePool, cycle_id: &str) -> Result<Vec<Finding>> {
    let rows = sqlx::query!(
        "SELECT source, severity, file, description, action FROM findings WHERE cycle_id = ? ORDER BY id ASC",
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

/// Persists one [`RunEvent`] (Phase 8, ADR-0008) as an `events` row. `id`
/// and `created_at` are supplied by the caller rather than generated here
/// (unlike most other `insert_*` functions in this module): the orchestrator
/// needs the *exact same* id/timestamp to also appear on the live Event Bus
/// broadcast (see `event_bus::EventBus::publish`), so a `warden-tui` that
/// subscribes to the bus before querying history can deduplicate an event it
/// already saw live against the same event showing up in a later history
/// query, by id.
pub async fn insert_event(
    pool: &SqlitePool,
    id: &str,
    run_id: &str,
    event: &RunEvent,
    created_at: &str,
) -> Result<()> {
    let event_type = event.kind().as_str();
    let payload_json = serde_json::to_string(event)?;
    sqlx::query!(
        "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
        id,
        run_id,
        event_type,
        payload_json,
        created_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A `evidence` row, with `type` already validated into
/// [`EvidenceType`] (ADR-0009, issue #7).
#[derive(Debug, Clone)]
pub struct Evidence {
    pub id: String,
    pub cycle_id: String,
    pub finding_id: Option<String>,
    pub evidence_type: EvidenceType,
    /// The eventual repo-relative destination path
    /// (`.warden/evidence/<cycle_number>/<filename>`) an artifact is stored
    /// under once committed (see `crate::evidence`) -- this column is
    /// written at capture time, before the commit itself happens, since it's
    /// deterministic and never changes (only the underlying bytes move from
    /// local scratch storage into the repo, at convergence).
    pub file_path: String,
    pub description: String,
    pub captured_at: String,
}

/// Records one artifact an evidence capture adapter produced for `cycle_id`
/// (ADR-0009). `finding_id` is `None` for the nominal case -- evidence
/// documenting that a cycle's behaviour works, not the resolution of one
/// specific finding.
#[allow(clippy::too_many_arguments)]
pub async fn insert_evidence(
    pool: &SqlitePool,
    id: &str,
    cycle_id: &str,
    finding_id: Option<&str>,
    evidence_type: EvidenceType,
    file_path: &str,
    description: &str,
) -> Result<()> {
    let now = now_rfc3339();
    let evidence_type = evidence_type.as_str();
    sqlx::query!(
        "INSERT INTO evidence (id, cycle_id, finding_id, type, file_path, description, captured_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        id,
        cycle_id,
        finding_id,
        evidence_type,
        file_path,
        description,
        now,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Raw shape of an `events` row as decoded by sqlx, before `event_type` and
/// `payload_json` have been validated into a [`RunEvent`]. Kept private:
/// [`RunEventRecord`] is the only form that ever leaves this module.
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
        return Err(WardenError::EventKindMismatch {
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

/// Every event recorded for `run_id`, oldest first -- the full history a
/// late-attaching `warden-tui` replays before switching to the live socket
/// stream (Architecture.md §5.4). Ordered by `created_at` then `id` so two
/// events sharing the same (second-resolution) timestamp still come back in
/// a stable, deterministic order rather than SQLite's unspecified row order.
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

/// One `evidence` row together with the `cycle_number` it belongs to -- the
/// bare `evidence` table only carries `cycle_id`, but `pr_summary`'s
/// Evidence section formatting (issue #7) groups/orders by cycle number.
pub struct EvidenceWithCycle {
    pub cycle_number: u32,
    pub evidence: Evidence,
}

/// Every evidence row captured across `run_id`'s cycles, ordered by cycle
/// then capture time -- used to build the Evidence section of the finalized
/// PR body (ADR-0009) and to find the artifacts still on local scratch
/// storage that need committing into the repo at convergence
/// (`evidence::commit_evidence_into_repo`).
pub async fn list_evidence_for_run(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Vec<EvidenceWithCycle>> {
    let rows = sqlx::query!(
        r#"
        SELECT evidence.id as "id!", evidence.cycle_id as "cycle_id!", evidence.finding_id,
               evidence.type as "evidence_type!", evidence.file_path as "file_path!",
               evidence.description as "description!", evidence.captured_at as "captured_at!",
               cycles.cycle_number as "cycle_number!"
        FROM evidence
        JOIN cycles ON cycles.id = evidence.cycle_id
        WHERE cycles.run_id = ?
        ORDER BY cycles.cycle_number ASC, evidence.captured_at ASC
        "#,
        run_id,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(EvidenceWithCycle {
                cycle_number: checked_u32(r.cycle_number, "cycles.cycle_number")?,
                evidence: Evidence {
                    id: r.id,
                    cycle_id: r.cycle_id,
                    finding_id: r.finding_id,
                    evidence_type: EvidenceType::parse(&r.evidence_type)?,
                    file_path: r.file_path,
                    description: r.description,
                    captured_at: r.captured_at,
                },
            })
        })
        .collect()
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
            RunState::RunningStep(1),
            RunState::RunningStep(2),
            RunState::RunningStep(7),
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
            RunState::Done,
            RunState::StepCyclesExceeded(1),
            RunState::StepCyclesExceeded(2),
            RunState::Failed,
        ] {
            let literal_says_intermediate = state.as_str() == "coder_running"
                || state.as_str() == "awaiting_ci"
                || state.as_str().starts_with("running_step:");
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
        insert_run(
            &pool,
            "run-1",
            "/tmp/repo",
            "main",
            "do the thing",
            5,
            4,
            3,
            5,
        )
        .await
        .unwrap();

        let run = get_run(&pool, "run-1").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Pending);
        assert_eq!(run.max_review_cycles, 5);
        assert_eq!(run.max_test_cycles, 4);
        assert_eq!(run.current_review_cycle, 0);
        assert_eq!(run.current_test_cycle, 0);
        assert_eq!(run.intent, "do the thing");
    }

    #[tokio::test]
    async fn pr_number_is_none_until_set_then_round_trips() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-pr", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
            .await
            .unwrap();

        let run = get_run(&pool, "run-pr").await.unwrap().unwrap();
        assert_eq!(run.pr_number, None);

        set_run_pr_number(&pool, "run-pr", 42).await.unwrap();

        let run = get_run(&pool, "run-pr").await.unwrap().unwrap();
        assert_eq!(run.pr_number, Some(42));
    }

    /// Issue #15 review, L2: an overflowing pr_number must be reported with
    /// its own real value, not a misleading placeholder like `i64::MAX`.
    #[tokio::test]
    async fn set_run_pr_number_overflow_reports_the_real_value() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-pr-overflow",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();

        let overflowing = u64::try_from(i64::MAX).unwrap() + 1;
        let result = set_run_pr_number(&pool, "run-pr-overflow", overflowing).await;

        assert!(matches!(
            result,
            Err(WardenError::PrNumberOverflow { pr_number }) if pr_number == overflowing
        ));
    }

    #[tokio::test]
    async fn update_run_state_persists_and_list_intermediate_finds_it() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, "run-2", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
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
        insert_run(&pool, "run-3", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
            .await
            .unwrap();
        update_run_state(&pool, "run-3", RunState::CoderRunning)
            .await
            .unwrap();
        update_run_state(&pool, "run-3", RunState::RunningStep(1))
            .await
            .unwrap();
        update_run_state(&pool, "run-3", RunState::RunningStep(2))
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
        insert_run(&pool, "run-4", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
            .await
            .unwrap();
        insert_cycle(&pool, "cycle-1", "run-4", 1).await.unwrap();
        set_cycle_worktree_path(&pool, "cycle-1", AgentRole::Coder, "/tmp/wt/coder")
            .await
            .unwrap();

        let finding = Finding {
            source: FindingSource::role("reviewer"),
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

    // -----------------------------------------------------------------
    // Token usage (issue #53)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn cycle_role_token_usage_is_none_until_something_is_recorded() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-usage-none",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-usage-none", "run-usage-none", 1)
            .await
            .unwrap();

        let usage = get_cycle_role_token_usage(&pool, "cycle-usage-none", AgentRole::Coder)
            .await
            .unwrap();
        assert_eq!(
            usage, None,
            "no usage was ever recorded -- must be n/a, not zero"
        );
    }

    #[tokio::test]
    async fn add_cycle_role_token_usage_accumulates_across_multiple_invocations() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-usage",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-usage", "run-usage", 1)
            .await
            .unwrap();

        add_cycle_role_token_usage(
            &pool,
            "cycle-usage",
            AgentRole::Coder,
            &TokenUsage::new(100, 50, Some(10), None),
        )
        .await
        .unwrap();
        add_cycle_role_token_usage(
            &pool,
            "cycle-usage",
            AgentRole::Coder,
            &TokenUsage::new(20, 10, None, Some(3)),
        )
        .await
        .unwrap();

        let usage = get_cycle_role_token_usage(&pool, "cycle-usage", AgentRole::Coder)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 60);
        // The second invocation didn't report `cache_read_tokens` -- must not
        // reset the first invocation's own reported total.
        assert_eq!(usage.cache_read_tokens, Some(10));
        assert_eq!(usage.cache_creation_tokens, Some(3));
    }

    /// Each role's own running total on the same cycle must be tracked
    /// independently -- recording the coder's usage must never leak into the
    /// reviewer's columns on that same row.
    #[tokio::test]
    async fn add_cycle_role_token_usage_keeps_each_role_independent() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-usage-roles",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-usage-roles", "run-usage-roles", 1)
            .await
            .unwrap();

        add_cycle_role_token_usage(
            &pool,
            "cycle-usage-roles",
            AgentRole::Coder,
            &TokenUsage::new(100, 50, None, None),
        )
        .await
        .unwrap();
        add_cycle_role_token_usage(
            &pool,
            "cycle-usage-roles",
            AgentRole::Reviewer,
            &TokenUsage::new(7, 3, None, None),
        )
        .await
        .unwrap();

        let coder_usage = get_cycle_role_token_usage(&pool, "cycle-usage-roles", AgentRole::Coder)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(coder_usage.input_tokens, 100);

        let tester_usage =
            get_cycle_role_token_usage(&pool, "cycle-usage-roles", AgentRole::Tester)
                .await
                .unwrap();
        assert_eq!(
            tester_usage, None,
            "the tester never ran on this cycle -- must stay n/a"
        );
    }

    #[tokio::test]
    async fn run_token_usage_is_none_until_something_is_recorded_then_accumulates() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-total-usage",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();

        assert_eq!(
            get_run_token_usage(&pool, "run-total-usage").await.unwrap(),
            None
        );

        add_run_token_usage(
            &pool,
            "run-total-usage",
            &TokenUsage::new(100, 50, Some(10), None),
        )
        .await
        .unwrap();
        add_run_token_usage(
            &pool,
            "run-total-usage",
            &TokenUsage::new(20, 10, Some(5), None),
        )
        .await
        .unwrap();

        let usage = get_run_token_usage(&pool, "run-total-usage")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 60);
        assert_eq!(usage.cache_read_tokens, Some(15));
    }

    /// Issue #53: a `u64` token count too large for SQLite's native `i64`
    /// column must surface as a typed `WardenError::TokenCountOverflow`
    /// naming the real value that failed to convert -- never silently
    /// truncated/clamped (same "no silent fallback" contract
    /// `set_run_pr_number_overflow_reports_the_real_value` already pins for
    /// `runs.pr_number`).
    #[tokio::test]
    async fn add_run_token_usage_overflow_reports_the_real_value() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-usage-overflow",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();

        let overflowing = u64::try_from(i64::MAX).unwrap() + 1;
        let result = add_run_token_usage(
            &pool,
            "run-usage-overflow",
            &TokenUsage::new(overflowing, 0, None, None),
        )
        .await;

        assert!(matches!(
            result,
            Err(WardenError::TokenCountOverflow { value, .. }) if value == overflowing
        ));
    }

    /// Same contract as
    /// [`add_run_token_usage_overflow_reports_the_real_value`], for the
    /// per-cycle-role columns [`add_cycle_role_token_usage`] writes.
    #[tokio::test]
    async fn add_cycle_role_token_usage_overflow_reports_the_real_value() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-cycle-usage-overflow",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-usage-overflow", "run-cycle-usage-overflow", 1)
            .await
            .unwrap();

        let overflowing = u64::try_from(i64::MAX).unwrap() + 1;
        let result = add_cycle_role_token_usage(
            &pool,
            "cycle-usage-overflow",
            AgentRole::Coder,
            &TokenUsage::new(overflowing, 0, None, None),
        )
        .await;

        assert!(matches!(
            result,
            Err(WardenError::TokenCountOverflow { value, .. }) if value == overflowing
        ));
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
        insert_run(&pool, "dup-run", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
            .await
            .unwrap();

        let result = insert_run(
            &pool,
            "dup-run",
            "/tmp/repo",
            "main",
            "intent again",
            3,
            3,
            3,
            5,
        )
        .await;
        assert!(matches!(result, Err(WardenError::Database(_))));

        // The original row must be untouched by the failed duplicate insert.
        let run = get_run(&pool, "dup-run").await.unwrap().unwrap();
        assert_eq!(run.intent, "intent");
    }

    #[tokio::test]
    async fn list_findings_for_cycle_with_no_findings_is_empty_not_an_error() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-empty",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-empty", "run-empty", 1)
            .await
            .unwrap();

        let findings = list_findings_for_cycle(&pool, "cycle-empty").await.unwrap();
        assert!(findings.is_empty());
    }

    /// Re-test cycle (issue #20 review fix, fdcaa4e): `ORDER BY id ASC`
    /// must actually determine the returned order, not merely happen to
    /// agree with insertion order. Deliberately inserts the
    /// lexicographically-later id first, so a query without the `ORDER BY`
    /// clause (which SQLite would otherwise satisfy via a plain rowid/
    /// insertion-order table scan here, since neither `cycle_id` nor `id`
    /// has a covering index driving this query) would return the rows in
    /// the opposite order from what's asserted here.
    #[tokio::test]
    async fn list_findings_for_cycle_orders_findings_by_id_ascending_not_insertion_order() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-order",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-order", "run-order", 1)
            .await
            .unwrap();

        let finding_z = Finding {
            source: FindingSource::role("reviewer"),
            severity: Severity::Blocking,
            file: None,
            description: "inserted first, id sorts last".to_string(),
            action: None,
        };
        let finding_a = Finding {
            source: FindingSource::role("tester"),
            severity: Severity::Blocking,
            file: None,
            description: "inserted second, id sorts first".to_string(),
            action: None,
        };

        insert_finding(&pool, "zzz-finding", "cycle-order", &finding_z)
            .await
            .unwrap();
        insert_finding(&pool, "aaa-finding", "cycle-order", &finding_a)
            .await
            .unwrap();

        let findings = list_findings_for_cycle(&pool, "cycle-order").await.unwrap();
        assert_eq!(
            findings,
            vec![finding_a, finding_z],
            "findings must be ordered by id ascending (aaa- before zzz-), regardless of \
             insertion order"
        );
    }

    #[tokio::test]
    async fn latest_open_agent_process_is_none_when_run_has_no_processes() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-no-proc",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
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
        insert_run(&pool, "run-5", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
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
        insert_run(&pool, "run-6", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
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
        insert_run(&pool, "run-7", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
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
        insert_run(&pool, "run-8", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
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
        insert_run(&pool, "run-9", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
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

    /// Issue #43 (#37.4): `0007_phase_budgets.sql` must not just add the new
    /// per-phase columns -- it also has to carry forward rows already
    /// sitting on the pre-#43 schema (single `max_cycles`/`current_cycle`,
    /// and `RunState` string values only the removed
    /// `AwaitingReviewTest`/`MaxCyclesExceeded` variants ever wrote:
    /// `awaiting_review_test`/`max_cycles_exceeded`). Every other test in
    /// this module goes through `connect`/`test_pool`, which always starts
    /// from an empty file and so applies migration 0007 against zero rows --
    /// it would pass even if the `UPDATE runs SET state = ...` remap
    /// statements were deleted entirely. This test instead seeds a row on
    /// the *pre-0007* schema (mirroring
    /// `connect_backs_up_a_pre_existing_database_before_applying_pending_migrations`'s
    /// `Migrator::run_to` technique to stop short of 0007), then lets
    /// `MIGRATOR.run` apply 0007 for real and checks the row lands exactly
    /// where the migration's own comments say it should.
    #[tokio::test]
    async fn phase_budgets_migration_remaps_pre_existing_rows_and_legacy_state_strings() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");

        // Issue #53 review: found by description, not by position relative
        // to the end of the migration list -- `migrations.len() - 2` (this
        // test's original technique) silently pointed at the wrong migration
        // the moment 0008 was appended after 0007, since "second-to-last"
        // stopped meaning "the one right before phase_budgets". Robust to any
        // number of migrations appended after 0007, as long as it keeps this
        // description.
        let migrations: Vec<_> = MIGRATOR.iter().collect();
        let phase_budgets_index = migrations
            .iter()
            .position(|migration| migration.description.contains("phase budgets"))
            .expect("0007_phase_budgets.sql must still be a migration in this set");
        let pre_phase_budgets_version = migrations[phase_budgets_index - 1].version;

        {
            let options = SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal);
            let pool = SqlitePoolOptions::new()
                .connect_with(options)
                .await
                .unwrap();

            MIGRATOR
                .run_to(pre_phase_budgets_version, &pool)
                .await
                .unwrap();

            // Two rows, each pinned on a distinct legacy `state` string the
            // migration must remap, both still on the single `max_cycles`/
            // `current_cycle` pair 0007 replaces.
            sqlx::query(
                "INSERT INTO runs (id, repo_path, branch, intent, state, max_cycles, current_cycle, created_at, updated_at) \
                 VALUES ('run-mid-cycle', '/tmp/repo', 'main', 'legacy mid-cycle run', 'awaiting_review_test', 5, 3, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO runs (id, repo_path, branch, intent, state, max_cycles, current_cycle, created_at, updated_at) \
                 VALUES ('run-exhausted', '/tmp/repo', 'main', 'legacy exhausted run', 'max_cycles_exceeded', 4, 4, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .execute(&pool)
            .await
            .unwrap();

            pool.close().await;
        }

        // Re-`connect` applies every remaining pending migration, including
        // 0007, against the seeded rows above.
        let pool = connect(&db_path).await.unwrap();

        let mid_cycle = get_run(&pool, "run-mid-cycle").await.unwrap().unwrap();
        assert_eq!(
            mid_cycle.state,
            RunState::RunningStep(1),
            "a legacy 'awaiting_review_test' row must remap onto 'reviewing' (0007), then onto \
             RunningStep(1) once 0009's own remap runs on top of it -- the specific phase can't \
             be recovered from the string alone, but every RunningStep index is equally \
             is_intermediate so crash recovery behaves the same regardless"
        );
        assert_eq!(
            mid_cycle.max_review_cycles, 5,
            "the old single max_cycles becomes both phases' starting budget"
        );
        assert_eq!(mid_cycle.max_test_cycles, 5);
        assert_eq!(
            mid_cycle.current_review_cycle, 3,
            "the old single current_cycle becomes the review phase's starting progress"
        );
        assert_eq!(
            mid_cycle.current_test_cycle, 0,
            "current_test_cycle has no legacy equivalent to carry forward, so it starts at 0"
        );

        let exhausted = get_run(&pool, "run-exhausted").await.unwrap().unwrap();
        assert_eq!(
            exhausted.state,
            RunState::StepCyclesExceeded(1),
            "a legacy 'max_cycles_exceeded' row must remap onto 'max_review_cycles_exceeded' \
             (0007), then onto StepCyclesExceeded(1) once 0009's own remap runs on top of it"
        );
        assert_eq!(exhausted.max_review_cycles, 4);
        assert_eq!(exhausted.max_test_cycles, 4);
        assert_eq!(exhausted.current_review_cycle, 4);
        assert_eq!(exhausted.current_test_cycle, 0);
    }

    /// Issue #6: "a failed backup aborts migration (fails loud) rather than
    /// proceeding". Forces `VACUUM INTO` to fail by revoking write
    /// permission on the directory the backup file would be created in
    /// *after* the pool (and its `-wal`/`-shm` sidecars) already exist —
    /// so the failure genuinely comes from the backup step itself, not from
    /// merely opening the database. `backup_before_migration` is private but
    /// reachable here via `super::*`, letting this test target the exact
    /// failure point without needing a full second `connect` (which would
    /// hit the same permission error earlier, at WAL setup, and not prove
    /// anything about the backup step specifically).
    #[cfg(unix)]
    #[tokio::test]
    async fn backup_failure_is_a_typed_error_not_a_silent_fallback() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");

        // Simulate an older Warden installation with only the first
        // migration applied, so a migration is genuinely pending and a
        // backup is attempted (mirrors the "pending migrations" test above).
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

        // Revoke write permission on the directory only now, after the pool
        // and its WAL sidecars already exist -- `VACUUM INTO` must fail
        // trying to create the *new* backup file in a directory it can no
        // longer write to.
        let original_permissions = std::fs::metadata(dir.path()).unwrap().permissions();
        let mut readonly = original_permissions.clone();
        readonly.set_mode(0o555);
        std::fs::set_permissions(dir.path(), readonly).unwrap();

        let result = backup_before_migration(&db_path, &pool).await;

        // Restore permissions before the TempDir is dropped, regardless of
        // the assertion outcome, so cleanup doesn't itself fail.
        std::fs::set_permissions(dir.path(), original_permissions).unwrap();

        assert!(
            matches!(result, Err(WardenError::Backup { .. })),
            "expected a typed Backup error when VACUUM INTO cannot write its target, got: {result:?}"
        );

        pool.close().await;
    }

    #[tokio::test]
    async fn unique_backup_path_appends_a_suffix_on_a_same_timestamp_collision() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let timestamp = "2026-07-11T00-00-00+00-00";

        // Nothing on disk yet: the plain, unsuffixed name is used.
        let first = unique_backup_path(&db_path, timestamp).await.unwrap();
        assert_eq!(first, dir.path().join(format!("state.db.bak-{timestamp}")));

        // Simulates a leftover/duplicate backup sharing the same timestamp
        // (e.g. two restarts within the same second) -- `VACUUM INTO` would
        // otherwise abort on a spurious naming collision rather than a real
        // backup failure.
        std::fs::write(&first, b"pre-existing backup").unwrap();
        let second = unique_backup_path(&db_path, timestamp).await.unwrap();
        assert_eq!(
            second,
            dir.path().join(format!("state.db.bak-{timestamp}-1"))
        );

        std::fs::write(&second, b"pre-existing backup").unwrap();
        let third = unique_backup_path(&db_path, timestamp).await.unwrap();
        assert_eq!(
            third,
            dir.path().join(format!("state.db.bak-{timestamp}-2"))
        );
    }

    #[tokio::test]
    async fn clear_cycle_worktree_path_nulls_out_only_the_given_role() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-clear",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-clear", "run-clear", 1)
            .await
            .unwrap();
        set_cycle_worktree_path(&pool, "cycle-clear", AgentRole::Coder, "/tmp/wt/coder")
            .await
            .unwrap();
        set_cycle_worktree_path(
            &pool,
            "cycle-clear",
            AgentRole::Reviewer,
            "/tmp/wt/reviewer",
        )
        .await
        .unwrap();

        clear_cycle_worktree_path(&pool, "cycle-clear", AgentRole::Coder)
            .await
            .unwrap();

        let entries = list_cycle_worktree_entries_for_run(&pool, "run-clear")
            .await
            .unwrap();
        assert_eq!(entries.len(), 1, "only the reviewer path should remain");
        assert_eq!(entries[0].role, AgentRole::Reviewer);
        assert_eq!(entries[0].path, "/tmp/wt/reviewer");
    }

    #[tokio::test]
    async fn failed_run_with_no_open_process_and_no_recorded_worktree_needs_no_cleanup() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-clean-failed",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        update_run_state(&pool, "run-clean-failed", RunState::CoderRunning)
            .await
            .unwrap();
        update_run_state(&pool, "run-clean-failed", RunState::Failed)
            .await
            .unwrap();

        let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
        assert!(
            pending.is_empty(),
            "a Failed run with nothing recorded to clean up must not be returned"
        );
    }

    #[tokio::test]
    async fn failed_run_with_an_open_agent_process_needs_cleanup() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-open-proc",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-open-proc", "run-open-proc", 1)
            .await
            .unwrap();
        insert_agent_process(
            &pool,
            "proc-open",
            "cycle-open-proc",
            AgentRole::Coder,
            999_999_998,
            "/tmp/wt",
        )
        .await
        .unwrap();
        update_run_state(&pool, "run-open-proc", RunState::Failed)
            .await
            .unwrap();

        let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "run-open-proc");
    }

    #[tokio::test]
    async fn failed_run_with_a_recorded_worktree_path_needs_cleanup() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-recorded-wt",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-recorded-wt", "run-recorded-wt", 1)
            .await
            .unwrap();
        set_cycle_worktree_path(
            &pool,
            "cycle-recorded-wt",
            AgentRole::Coder,
            "/tmp/wt/coder",
        )
        .await
        .unwrap();
        update_run_state(&pool, "run-recorded-wt", RunState::Failed)
            .await
            .unwrap();

        let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "run-recorded-wt");

        // Once the path is cleared (simulating a successful removal), the
        // run must stop being returned -- no separate "cleanup done" flag,
        // the recorded path itself is the signal.
        clear_cycle_worktree_path(&pool, "cycle-recorded-wt", AgentRole::Coder)
            .await
            .unwrap();
        let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
        assert!(pending.is_empty());
    }

    // -----------------------------------------------------------------
    // EVIDENCE entity (ADR-0009, issue #7): insert + query back
    // (migration 0004_evidence).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn evidence_round_trips_through_insert_and_list_evidence_for_run() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-evidence",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-evidence", "run-evidence", 1)
            .await
            .unwrap();

        insert_evidence(
            &pool,
            "evidence-1",
            "cycle-evidence",
            None,
            EvidenceType::Image,
            ".warden/evidence/1/screenshot.png",
            "Playwright capture from the cycle's e2e test run",
        )
        .await
        .unwrap();

        let evidence = list_evidence_for_run(&pool, "run-evidence").await.unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].cycle_number, 1);
        assert_eq!(evidence[0].evidence.id, "evidence-1");
        assert_eq!(evidence[0].evidence.cycle_id, "cycle-evidence");
        assert_eq!(evidence[0].evidence.finding_id, None);
        assert_eq!(evidence[0].evidence.evidence_type, EvidenceType::Image);
        assert_eq!(
            evidence[0].evidence.file_path,
            ".warden/evidence/1/screenshot.png"
        );
        assert_eq!(
            evidence[0].evidence.description,
            "Playwright capture from the cycle's e2e test run"
        );
    }

    #[tokio::test]
    async fn evidence_can_be_linked_to_a_specific_finding() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-evidence-finding",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-evidence-finding", "run-evidence-finding", 1)
            .await
            .unwrap();
        let finding = Finding {
            source: FindingSource::role("tester"),
            severity: Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "flaky button".to_string(),
            action: Some("fix it".to_string()),
        };
        insert_finding(
            &pool,
            "finding-evidence",
            "cycle-evidence-finding",
            &finding,
        )
        .await
        .unwrap();

        insert_evidence(
            &pool,
            "evidence-linked",
            "cycle-evidence-finding",
            Some("finding-evidence"),
            EvidenceType::Video,
            ".warden/evidence/1/failure.webm",
            "video of the observed failure",
        )
        .await
        .unwrap();

        let evidence = list_evidence_for_run(&pool, "run-evidence-finding")
            .await
            .unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(
            evidence[0].evidence.finding_id.as_deref(),
            Some("finding-evidence")
        );
    }

    #[tokio::test]
    async fn list_evidence_for_run_is_empty_when_no_evidence_was_captured() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-no-evidence",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-no-evidence", "run-no-evidence", 1)
            .await
            .unwrap();

        let evidence = list_evidence_for_run(&pool, "run-no-evidence")
            .await
            .unwrap();
        assert!(evidence.is_empty());
    }

    #[tokio::test]
    async fn list_evidence_for_run_orders_by_cycle_number_then_capture_time() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-evidence-order",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        insert_cycle(&pool, "cycle-2", "run-evidence-order", 2)
            .await
            .unwrap();
        insert_cycle(&pool, "cycle-1", "run-evidence-order", 1)
            .await
            .unwrap();

        // Deliberately inserted out of cycle order: cycle 2's evidence
        // lands in the table first, but must still be listed *after*
        // cycle 1's when read back.
        insert_evidence(
            &pool,
            "evidence-cycle-2",
            "cycle-2",
            None,
            EvidenceType::Other,
            ".warden/evidence/2/session.cast",
            "cycle 2 recording",
        )
        .await
        .unwrap();
        insert_evidence(
            &pool,
            "evidence-cycle-1",
            "cycle-1",
            None,
            EvidenceType::Other,
            ".warden/evidence/1/session.cast",
            "cycle 1 recording",
        )
        .await
        .unwrap();

        let evidence = list_evidence_for_run(&pool, "run-evidence-order")
            .await
            .unwrap();
        assert_eq!(evidence.len(), 2);
        assert_eq!(evidence[0].cycle_number, 1);
        assert_eq!(evidence[1].cycle_number, 2);
    }

    #[tokio::test]
    async fn intermediate_runs_are_not_returned_by_the_failed_cleanup_query() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-still-running",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        update_run_state(&pool, "run-still-running", RunState::CoderRunning)
            .await
            .unwrap();

        let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
        assert!(
            pending.is_empty(),
            "a run that isn't Failed yet belongs to list_intermediate_runs, not this query"
        );
    }

    // ---- events (Phase 8, issue #8) ----------------------------------------

    #[tokio::test]
    async fn event_round_trips_through_insert_and_list() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-events",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();

        let event = RunEvent::CycleStarted { cycle_number: 1 };
        insert_event(
            &pool,
            "event-1",
            "run-events",
            &event,
            "2026-07-12T00:00:00+00:00",
        )
        .await
        .unwrap();

        let events = list_events_for_run(&pool, "run-events").await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "event-1");
        assert_eq!(events[0].run_id, "run-events");
        assert_eq!(events[0].event, event);
        assert_eq!(events[0].created_at, "2026-07-12T00:00:00+00:00");
    }

    #[tokio::test]
    async fn list_events_for_run_orders_oldest_first() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-order",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();

        insert_event(
            &pool,
            "event-b",
            "run-order",
            &RunEvent::CycleStarted { cycle_number: 2 },
            "2026-07-12T00:00:02+00:00",
        )
        .await
        .unwrap();
        insert_event(
            &pool,
            "event-a",
            "run-order",
            &RunEvent::CycleStarted { cycle_number: 1 },
            "2026-07-12T00:00:01+00:00",
        )
        .await
        .unwrap();

        let events = list_events_for_run(&pool, "run-order").await.unwrap();
        let ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["event-a", "event-b"]);
    }

    #[tokio::test]
    async fn list_events_for_run_is_empty_for_a_run_with_no_events() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-no-events",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();

        let events = list_events_for_run(&pool, "run-no-events").await.unwrap();
        assert!(events.is_empty());
    }

    /// code-standards.md: "toute ligne relue est reparsée en type Rust
    /// fort" -- a row whose `event_type` column disagrees with what its own
    /// `payload_json` decodes to (corruption, or a write from something
    /// other than `insert_event`) must be a typed error, never silently
    /// trusted as whichever of the two the reader happens to pick.
    #[tokio::test]
    async fn mismatched_event_type_and_payload_kind_is_a_typed_error_not_silently_trusted() {
        let (_dir, pool) = test_pool().await;
        insert_run(
            &pool,
            "run-corrupt",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();

        let payload_json =
            serde_json::to_string(&RunEvent::CycleStarted { cycle_number: 1 }).unwrap();
        sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            "event-corrupt",
            "run-corrupt",
            "run_finished",
            payload_json,
            "2026-07-12T00:00:00+00:00",
        )
        .execute(&pool)
        .await
        .unwrap();

        let result = list_events_for_run(&pool, "run-corrupt").await;
        assert!(matches!(result, Err(WardenError::EventKindMismatch { .. })));
    }
}
