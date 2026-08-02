use super::*;

/// A `runs` row, with `state` already validated into [`RunState`].
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
    /// how many steps this run's own resolved `warden_core::Workflow` has (`workflow.steps.len()`).
    pub total_steps: u32,
    pub max_extra_step_cycles: u32,
    pub current_extra_step_cycle: u32,
    pub created_at: String,
    pub updated_at: String,
    pub converged_commit_sha: Option<String>,
    /// The PR `warden-gated` opened for this run — `None` until `Pushed`'s tail successfully opens
    /// one.
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

pub async fn update_run_state(pool: &SqlitePool, run_id: &str, new_state: RunState) -> Result<()> {
    let now = now_rfc3339();
    let state = new_state.as_str();
    let quota_resets_at = match new_state {
        RunState::AwaitingQuotaReset { resets_at } => Some(resets_at),
        _ => None,
    };
    if new_state == RunState::Failed {
        sqlx::query!(
            "UPDATE runs SET state = ?, quota_resets_at = ?, quota_resume_owner_pid = NULL, \
             quota_resume_owner_started_at_unix = NULL, quota_resume_claimed_at = NULL, \
             updated_at = ? WHERE id = ?",
            state,
            quota_resets_at,
            now,
            run_id,
        )
        .execute(pool)
        .await?;
    } else {
        sqlx::query!(
            "UPDATE runs SET state = ?, quota_resets_at = ?, updated_at = ? WHERE id = ?",
            state,
            quota_resets_at,
            now,
            run_id,
        )
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn fail_run_with_pending_cleanup(pool: &SqlitePool, run_id: &str) -> Result<()> {
    let now = now_rfc3339();
    let mut transaction = pool.begin().await?;
    sqlx::query!(
        "UPDATE runs SET state = ?, quota_resets_at = NULL, quota_resume_owner_pid = NULL, \
         quota_resume_owner_started_at_unix = NULL, quota_resume_claimed_at = NULL, \
         updated_at = ? WHERE id = ?",
        RunState::Failed.as_str(),
        now,
        run_id,
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query!(
        "INSERT OR IGNORE INTO run_cleanup_queue (run_id) VALUES (?)",
        run_id,
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn mark_run_cleanup_pending(pool: &SqlitePool, run_id: &str) -> Result<()> {
    sqlx::query!(
        "INSERT OR IGNORE INTO run_cleanup_queue (run_id) VALUES (?)",
        run_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn clear_run_cleanup_pending(pool: &SqlitePool, run_id: &str) -> Result<()> {
    sqlx::query!("DELETE FROM run_cleanup_queue WHERE run_id = ?", run_id)
        .execute(pool)
        .await?;
    Ok(())
}

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

/// Records the commit SHA a run converged on (M4).
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

/// Records the PR `warden-gated` opened for this run, once the post-Converged tail's `OpenDraft`
/// succeeds.
pub async fn set_run_pr_number(pool: &SqlitePool, run_id: &str, pr_number: u64) -> Result<()> {
    let now = now_rfc3339();
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

/// accumulates one agent invocation's token usage onto `run_id`'s running total.
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

/// The run total accumulated so far by [`add_run_token_usage`], or `None` if this run's tool never
/// reported any usage at all (rendered "n/a" by every caller, never `0`.
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

/// Raw shape of a `runs` row as decoded by sqlx, before `state` has been validated into a
/// [`RunState`].
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

/// Runs left in an intermediate state (`RunState::is_intermediate`) as of the last shutdown/crash.
pub async fn list_intermediate_runs(pool: &SqlitePool) -> Result<Vec<Run>> {
    let rows = sqlx::query_as!(
        RunRow,
        r#"
        SELECT id as "id!", repo_path, branch, intent, state, max_review_cycles, max_test_cycles, current_review_cycle, current_test_cycle, total_steps, max_extra_step_cycles, current_extra_step_cycle, created_at, updated_at, converged_commit_sha, pr_number
        FROM runs
        WHERE state IN ('coder_running', 'awaiting_ci', 'resuming_quota') OR state LIKE 'running_step:%'
        "#
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(row_to_run).collect()
}

pub async fn list_failed_runs_with_pending_cleanup(pool: &SqlitePool) -> Result<Vec<Run>> {
    let rows = sqlx::query_as!(
        RunRow,
        r#"
        SELECT DISTINCT runs.id as "id!", runs.repo_path, runs.branch, runs.intent, runs.state, runs.max_review_cycles, runs.max_test_cycles, runs.current_review_cycle, runs.current_test_cycle, runs.total_steps, runs.max_extra_step_cycles, runs.current_extra_step_cycle, runs.created_at, runs.updated_at, runs.converged_commit_sha, runs.pr_number
        FROM runs
        WHERE runs.state = 'failed'
          AND (
            EXISTS (
                SELECT 1 FROM run_cleanup_queue
                WHERE run_cleanup_queue.run_id = runs.id
            )
            OR EXISTS (
                SELECT 1 FROM agent_processes
                JOIN cycles ON cycles.id = agent_processes.cycle_id
                WHERE cycles.run_id = runs.id AND agent_processes.ended_at IS NULL
            )
            OR EXISTS (
                SELECT 1 FROM cycle_worktrees
                JOIN cycles ON cycles.id = cycle_worktrees.cycle_id
                WHERE cycles.run_id = runs.id
            )
          )
        "#
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(row_to_run).collect()
}
