use super::*;

pub async fn set_run_rate_limit_status(
    pool: &SqlitePool,
    run_id: &str,
    status: &RateLimitStatus,
) -> Result<()> {
    let status_str = status.status.as_str();
    let rate_limit_type_str = status.rate_limit_type.as_str();
    let is_using_overage = i64::from(status.is_using_overage);
    let now = now_rfc3339();
    sqlx::query!(
        r#"
        UPDATE runs SET
            rate_limit_status = ?,
            rate_limit_type = ?,
            rate_limit_utilization = ?,
            rate_limit_is_using_overage = ?,
            rate_limit_surpassed_threshold = ?,
            rate_limit_resets_at = ?,
            updated_at = ?
        WHERE id = ?
        "#,
        status_str,
        rate_limit_type_str,
        status.utilization,
        is_using_overage,
        status.surpassed_threshold,
        status.resets_at,
        now,
        run_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_run_rate_limit_status(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Option<RateLimitStatus>> {
    let row = sqlx::query!(
        r#"
        SELECT rate_limit_status, rate_limit_type, rate_limit_utilization, rate_limit_is_using_overage, rate_limit_surpassed_threshold, rate_limit_resets_at
        FROM runs WHERE id = ?
        "#,
        run_id,
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    row_to_rate_limit_status(
        run_id,
        row.rate_limit_status,
        row.rate_limit_type,
        row.rate_limit_utilization,
        row.rate_limit_is_using_overage,
        row.rate_limit_surpassed_threshold,
        row.rate_limit_resets_at,
    )
}

/// Removes a quota observation once its reset time has passed and is about to retry the recorded
/// workflow boundary.
pub async fn clear_run_rate_limit_status(pool: &SqlitePool, run_id: &str) -> Result<()> {
    let now = now_rfc3339();
    sqlx::query!(
        r#"
        UPDATE runs SET
            rate_limit_status = NULL,
            rate_limit_type = NULL,
            rate_limit_utilization = NULL,
            rate_limit_is_using_overage = NULL,
            rate_limit_surpassed_threshold = NULL,
            rate_limit_resets_at = NULL,
            updated_at = ?
        WHERE id = ?
        "#,
        now,
        run_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The two validated JSON documents needed to reconstruct one suspended convergence loop.
#[derive(Debug)]
pub struct QuotaContinuationRecord {
    pub config_json: String,
    pub state_json: String,
}

/// Atomically records both the continuation and the state transition that makes it eligible for
/// startup resumption.
pub async fn suspend_run_with_quota_continuation(
    pool: &SqlitePool,
    run_id: &str,
    resets_at: i64,
    config_json: &str,
    state_json: &str,
) -> Result<()> {
    let state = RunState::AwaitingQuotaReset { resets_at }.as_str();
    let now = now_rfc3339();
    let mut transaction = pool.begin().await?;
    sqlx::query!(
        "INSERT INTO quota_continuations (run_id, config_json, state_json, updated_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT (run_id) DO UPDATE SET \
             config_json = excluded.config_json, \
             state_json = excluded.state_json, \
             updated_at = excluded.updated_at",
        run_id,
        config_json,
        state_json,
        now,
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query!(
        "UPDATE runs SET state = ?, quota_resets_at = ?, quota_resume_owner_pid = NULL, \
         quota_resume_owner_started_at_unix = NULL, quota_resume_claimed_at = NULL, \
         updated_at = ? WHERE id = ?",
        state,
        resets_at,
        now,
        run_id,
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

/// One exact generation of a quota wait observed by a startup recovery pass.
#[derive(Debug, Clone)]
pub struct DueQuotaContinuation {
    pub run_id: String,
    expected_state: String,
    expected_resets_at: i64,
    expected_updated_at: String,
}

/// Materializes quota continuations due as of `now_unix` without claiming them.
pub async fn list_due_quota_continuations(
    pool: &SqlitePool,
    now_unix: i64,
) -> Result<Vec<DueQuotaContinuation>> {
    let rows = sqlx::query!(
        r#"
        SELECT id as "id!", state as "state!", quota_resets_at as "quota_resets_at!", updated_at as "updated_at!"
        FROM runs
        WHERE state LIKE 'awaiting_quota_reset:%'
          AND quota_resets_at IS NOT NULL
          AND quota_resets_at <= ?
        ORDER BY quota_resets_at, created_at, id
        "#,
        now_unix,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| DueQuotaContinuation {
            run_id: row.id,
            expected_state: row.state,
            expected_resets_at: row.quota_resets_at,
            expected_updated_at: row.updated_at,
        })
        .collect())
}

/// Atomically changes one exact quota-wait generation to `resuming_quota`.
pub async fn claim_due_quota_continuation(
    pool: &SqlitePool,
    candidate: &DueQuotaContinuation,
    now_unix: i64,
) -> Result<bool> {
    let claimed_state = RunState::ResumingQuota.as_str();
    let claimed_at = now_rfc3339();
    let owner_pid = std::process::id();
    let owner_started_at_unix = crate::process::process_start_time(owner_pid)
        .ok_or(WardenError::MissingQuotaResumeLeaseFingerprint { pid: owner_pid })?;
    let owner_pid = i64::from(owner_pid);
    let result = sqlx::query!(
        r#"
        UPDATE runs
        SET state = ?, quota_resets_at = NULL, quota_resume_owner_pid = ?,
            quota_resume_owner_started_at_unix = ?, quota_resume_claimed_at = ?, updated_at = ?
        WHERE id = ?
          AND state = ?
          AND quota_resets_at = ?
          AND quota_resets_at <= ?
          AND updated_at = ?
        "#,
        claimed_state,
        owner_pid,
        owner_started_at_unix,
        claimed_at,
        claimed_at,
        candidate.run_id,
        candidate.expected_state,
        candidate.expected_resets_at,
        now_unix,
        candidate.expected_updated_at,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// The Warden process that currently owns a `ResumingQuota` claim.
#[derive(Debug, Clone, Copy)]
pub struct QuotaResumeLease {
    pub owner_pid: u32,
    pub owner_started_at_unix: i64,
}

/// Reads a quota-resume lease at the persistence boundary.
pub async fn get_quota_resume_lease(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Option<QuotaResumeLease>> {
    let row = sqlx::query!(
        r#"
        SELECT quota_resume_owner_pid, quota_resume_owner_started_at_unix,
               quota_resume_claimed_at
        FROM runs
        WHERE id = ?
        "#,
        run_id,
    )
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    match (
        row.quota_resume_owner_pid,
        row.quota_resume_owner_started_at_unix,
        row.quota_resume_claimed_at,
    ) {
        (None, None, None) => Ok(None),
        (Some(pid), Some(owner_started_at_unix), Some(_)) => Ok(Some(QuotaResumeLease {
            owner_pid: checked_u32(pid, "runs.quota_resume_owner_pid")?,
            owner_started_at_unix,
        })),
        _ => Err(WardenError::InvalidQuotaContinuation {
            run_id: run_id.to_string(),
            reason: "quota-resume lease fields are only partially populated".to_string(),
        }),
    }
}

/// Atomically fails a claimed quota resume before it has handed off to an agent process.
pub async fn fail_quota_resume_claim(pool: &SqlitePool, run_id: &str) -> Result<bool> {
    let now = now_rfc3339();
    let mut transaction = pool.begin().await?;
    let result = sqlx::query!(
        "UPDATE runs SET state = ?, quota_resets_at = NULL, quota_resume_owner_pid = NULL, \
         quota_resume_owner_started_at_unix = NULL, quota_resume_claimed_at = NULL, \
         updated_at = ? WHERE id = ? AND quota_resume_owner_pid IS NOT NULL",
        RunState::Failed.as_str(),
        now,
        run_id,
    )
    .execute(&mut *transaction)
    .await?;
    if result.rows_affected() == 1 {
        sqlx::query!("DELETE FROM quota_continuations WHERE run_id = ?", run_id)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(result.rows_affected() == 1)
}

pub async fn get_quota_continuation(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Option<QuotaContinuationRecord>> {
    let row = sqlx::query!(
        "SELECT config_json as \"config_json!\", state_json as \"state_json!\" \
         FROM quota_continuations WHERE run_id = ?",
        run_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| QuotaContinuationRecord {
        config_json: row.config_json,
        state_json: row.state_json,
    }))
}

pub async fn delete_quota_continuation(pool: &SqlitePool, run_id: &str) -> Result<()> {
    sqlx::query!("DELETE FROM quota_continuations WHERE run_id = ?", run_id,)
        .execute(pool)
        .await?;
    Ok(())
}

/// Validates that an active-cycle id restored from a checkpoint still names a cycle belonging to
/// the same run.
pub async fn cycle_belongs_to_run(pool: &SqlitePool, cycle_id: &str, run_id: &str) -> Result<bool> {
    let exists = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM cycles WHERE id = ? AND run_id = ?)",
        cycle_id,
        run_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(exists != 0)
}

/// Converts the six possibly-`NULL` `rate_limit_*` columns read back from `runs` into
/// `Option<RateLimitStatus>`.
fn row_to_rate_limit_status(
    run_id: &str,
    status: Option<String>,
    rate_limit_type: Option<String>,
    utilization: Option<f64>,
    is_using_overage: Option<i64>,
    surpassed_threshold: Option<f64>,
    resets_at: Option<i64>,
) -> Result<Option<RateLimitStatus>> {
    match (
        status,
        rate_limit_type,
        utilization,
        is_using_overage,
        surpassed_threshold,
        resets_at,
    ) {
        (None, None, None, None, None, None) => Ok(None),
        (
            Some(status),
            Some(rate_limit_type),
            Some(utilization),
            Some(is_using_overage),
            Some(surpassed_threshold),
            Some(resets_at),
        ) => Ok(Some(RateLimitStatus::new(
            RateLimitState::from(status),
            RateLimitWindow::from(rate_limit_type),
            utilization,
            is_using_overage != 0,
            surpassed_threshold,
            resets_at,
        ))),
        _ => Err(WardenError::CorruptRateLimitStatusRow {
            run_id: run_id.to_string(),
        }),
    }
}
