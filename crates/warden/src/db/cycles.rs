use super::*;

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

/// Issue #73 follow-up (trio-unification): records/overwrites the worktree
/// path for `role` on `cycle_id` -- generalized from three hardcoded
/// `cycles.{coder,reviewer,tester}_worktree_path` columns to one row per
/// (cycle, role) in `cycle_worktrees` (migrations/0010), open to any role a
/// workflow declares. Every step, built-in or custom, calls this now -- see
/// `warden::orchestrator::InvocationRole`'s removal: there is no longer a
/// role for which this bookkeeping is skipped.
pub async fn set_cycle_worktree_path(
    pool: &SqlitePool,
    cycle_id: &str,
    role: &str,
    path: &str,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO cycle_worktrees (cycle_id, role, worktree_path) VALUES (?, ?, ?) \
         ON CONFLICT (cycle_id, role) DO UPDATE SET worktree_path = excluded.worktree_path",
        cycle_id,
        role,
        path,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes the recorded worktree path row for `role` on `cycle_id`, once
/// crash recovery has actually removed that worktree from disk (issue #6).
/// This is what lets [`list_failed_runs_with_pending_cleanup`] stop
/// returning a run after its orphan cleanup succeeds — the run stays
/// `Failed` forever (a terminal state), but the *recorded row* is the
/// signal that tells a later recovery pass whether there is still anything
/// left to reclaim for it.
pub async fn clear_cycle_worktree_path(
    pool: &SqlitePool,
    cycle_id: &str,
    role: &str,
) -> Result<()> {
    sqlx::query!(
        "DELETE FROM cycle_worktrees WHERE cycle_id = ? AND role = ?",
        cycle_id,
        role,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Issue #53 (generalized by the trio-unification follow-up, issue #73):
/// accumulates one agent invocation's token usage onto `role`'s running
/// total for this cycle -- the cycle-level half of the aggregation (see
/// [`add_run_token_usage`]'s own docs for the run-level half and the shared
/// "cache columns only advance when reported" rule both follow). One
/// upserted row per (cycle, role) in `cycle_token_usage` (migrations/0010)
/// rather than three hardcoded column groups -- open to any role.
pub async fn add_cycle_role_token_usage(
    pool: &SqlitePool,
    cycle_id: &str,
    role: &str,
    usage: &TokenUsage,
) -> Result<()> {
    let input_tokens = checked_i64(usage.input_tokens, "cycle_token_usage.input_tokens")?;
    let output_tokens = checked_i64(usage.output_tokens, "cycle_token_usage.output_tokens")?;
    let cache_read_tokens = usage
        .cache_read_tokens
        .map(|value| checked_i64(value, "cycle_token_usage.cache_read_tokens"))
        .transpose()?;
    let cache_creation_tokens = usage
        .cache_creation_tokens
        .map(|value| checked_i64(value, "cycle_token_usage.cache_creation_tokens"))
        .transpose()?;

    sqlx::query!(
        r#"
        INSERT INTO cycle_token_usage (cycle_id, role, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT (cycle_id, role) DO UPDATE SET
            input_tokens = COALESCE(cycle_token_usage.input_tokens, 0) + excluded.input_tokens,
            output_tokens = COALESCE(cycle_token_usage.output_tokens, 0) + excluded.output_tokens,
            cache_read_tokens = CASE WHEN excluded.cache_read_tokens IS NULL THEN cycle_token_usage.cache_read_tokens ELSE COALESCE(cycle_token_usage.cache_read_tokens, 0) + excluded.cache_read_tokens END,
            cache_creation_tokens = CASE WHEN excluded.cache_creation_tokens IS NULL THEN cycle_token_usage.cache_creation_tokens ELSE COALESCE(cycle_token_usage.cache_creation_tokens, 0) + excluded.cache_creation_tokens END
        "#,
        cycle_id,
        role,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The running total accumulated by [`add_cycle_role_token_usage`] for
/// `role` on `cycle_id`, or `None` if that role never reported any usage on
/// this cycle (e.g. it hasn't run yet, or its tool reports no usage at all
/// -- rendered "n/a" by every caller, see [`row_to_token_usage`]).
pub async fn get_cycle_role_token_usage(
    pool: &SqlitePool,
    cycle_id: &str,
    role: &str,
) -> Result<Option<TokenUsage>> {
    let row = sqlx::query!(
        r#"
        SELECT input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens
        FROM cycle_token_usage WHERE cycle_id = ? AND role = ?
        "#,
        cycle_id,
        role,
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    row_to_token_usage(
        row.input_tokens,
        row.output_tokens,
        row.cache_read_tokens,
        row.cache_creation_tokens,
    )
}

pub async fn close_cycle(pool: &SqlitePool, cycle_id: &str) -> Result<()> {
    let now = now_rfc3339();
    sqlx::query!("UPDATE cycles SET ended_at = ? WHERE id = ?", now, cycle_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The distinct worktree paths recorded across every cycle of `run_id`
/// (`cycle_worktrees`, migrations/0010). Used by crash recovery to find
/// worktrees that may have been orphaned when the orchestrator that owned
/// them died before it could call `Worktree::remove` (issue #6) -- every
/// role's, built-in or custom, since issue #73's trio-unification follow-up.
pub async fn list_worktree_paths_for_run(pool: &SqlitePool, run_id: &str) -> Result<Vec<String>> {
    let rows = sqlx::query!(
        r#"
        SELECT DISTINCT cycle_worktrees.worktree_path as "worktree_path!"
        FROM cycle_worktrees
        JOIN cycles ON cycles.id = cycle_worktrees.cycle_id
        WHERE cycles.run_id = ?
        "#,
        run_id,
    )
    .fetch_all(pool)
    .await?;

    let mut paths: Vec<String> = rows.into_iter().map(|row| row.worktree_path).collect();
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
    pub role: String,
    pub path: String,
}

/// Every worktree path recorded across `run_id`'s cycles, tagged with the
/// cycle/role it came from. Used by crash recovery so a successfully
/// removed worktree's path can be cleared afterwards (issue #6): without
/// that, a `Failed` run would look like it still has orphaned worktrees
/// forever, since the row is otherwise never cleared once a cycle records
/// it. Every role's, built-in or custom (issue #73's trio-unification
/// follow-up) -- no step's worktree leaks on crash anymore.
pub async fn list_cycle_worktree_entries_for_run(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Vec<CycleWorktreeEntry>> {
    let rows = sqlx::query!(
        r#"
        SELECT cycle_worktrees.cycle_id as "cycle_id!", cycle_worktrees.role as "role!", cycle_worktrees.worktree_path as "worktree_path!"
        FROM cycle_worktrees
        JOIN cycles ON cycles.id = cycle_worktrees.cycle_id
        WHERE cycles.run_id = ?
        "#,
        run_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| CycleWorktreeEntry {
            cycle_id: row.cycle_id,
            role: row.role,
            path: row.worktree_path,
        })
        .collect())
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
