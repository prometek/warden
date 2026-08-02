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

/// Records the commit SHA produced during this cycle.
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

/// Removes the recorded worktree path row for `role` on `cycle_id`, once crash recovery has
/// actually removed that worktree from disk.
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

/// The distinct worktree paths recorded across every cycle of `run_id` (`cycle_worktrees`,
/// migrations/0010).
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

pub struct CycleWorktreeEntry {
    pub cycle_id: String,
    pub role: String,
    pub path: String,
}

/// Every worktree path recorded across `run_id`'s cycles, tagged with the cycle/role it came from.
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
