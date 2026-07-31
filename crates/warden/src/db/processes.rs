use super::*;

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
    role: &str,
    pid: u32,
    worktree_path: &str,
) -> Result<()> {
    let now = now_rfc3339();
    let pid_started_at_unix =
        crate::process::process_start_time(pid).unwrap_or(crate::process::UNKNOWN_START_TIME);
    let pid = i64::from(pid);
    let mut transaction = pool.begin().await?;
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
    .execute(&mut *transaction)
    .await?;
    sqlx::query!(
        "UPDATE runs SET quota_resume_owner_pid = NULL, quota_resume_owner_started_at_unix = NULL, \
         quota_resume_claimed_at = NULL WHERE id = (SELECT run_id FROM cycles WHERE id = ?)",
        cycle_id,
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
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
