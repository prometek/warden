use super::gate_tail::delete_gate_staging_ref;
use super::*;

/// Crash recovery (Architecture.md §6 "Règle de récupération" / §9 Disaster Recovery).
pub async fn recover_crashed_runs(pool: &SqlitePool) -> Result<Vec<String>> {
    recover_crashed_runs_with(pool, reclaim_run_containers).await
}

async fn reclaim_run_containers(run_id: String) -> warden_sandbox::Result<usize> {
    warden_sandbox::reclaim_run_containers(&run_id).await
}

async fn recover_crashed_runs_with<F, Fut>(
    pool: &SqlitePool,
    reclaim_containers: F,
) -> Result<Vec<String>>
where
    F: Fn(String) -> Fut + Copy,
    Fut: std::future::Future<Output = warden_sandbox::Result<usize>>,
{
    let intermediate_runs = db::list_intermediate_runs(pool).await?;
    let mut failed_run_ids = Vec::new();

    for run in intermediate_runs {
        if run.state == RunState::AwaitingCi {
            continue;
        }

        if let Some(lease) = db::get_quota_resume_lease(pool, &run.id).await? {
            let has_live_resume_lease =
                process::is_process_alive(lease.owner_pid, lease.owner_started_at_unix);
            if has_live_resume_lease {
                tracing::info!(run_id = %run.id, state = run.state.as_str(), "quota resume has a live owner lease; leaving state untouched");
                continue;
            }
            tracing::warn!(run_id = %run.id, "quota resume claim has no live owner lease; recovering as crashed");
        }

        let open_process = db::latest_open_agent_process_for_run(pool, &run.id).await?;
        let has_live_process = open_process
            .map(|p| process::is_process_alive(p.pid, p.pid_started_at_unix))
            .unwrap_or(false);

        if has_live_process {
            tracing::info!(run_id = %run.id, "intermediate run has a live process; leaving state untouched");
            continue;
        }

        run.state
            .validate_transition(RunState::Failed, run.total_steps)?;
        db::fail_run_with_pending_cleanup(pool, &run.id).await?;
        db::delete_quota_continuation(pool, &run.id).await?;
        tracing::warn!(run_id = %run.id, previous_state = run.state.as_str(), "run recovered as Failed: no live process found");

        reclaim_orphan_resources_with(pool, &run, reclaim_containers).await?;
        failed_run_ids.push(run.id);
    }

    let failed_runs_needing_cleanup = db::list_failed_runs_with_pending_cleanup(pool).await?;
    for run in failed_runs_needing_cleanup {
        tracing::warn!(run_id = %run.id, "resuming orphan cleanup for a run already marked Failed by an earlier, interrupted recovery pass");
        reclaim_orphan_resources_with(pool, &run, reclaim_containers).await?;
    }

    Ok(failed_run_ids)
}

pub async fn resume_awaiting_ci_runs<G: GateTrigger>(
    pool: SqlitePool,
    warden_home: PathBuf,
    trigger: G,
    bare_repo_path: PathBuf,
) -> Result<Vec<String>> {
    let intermediate_runs = db::list_intermediate_runs(&pool).await?;
    let orchestrator = Orchestrator::new(pool.clone());
    let mut resumed_run_ids = Vec::new();

    for run in intermediate_runs {
        if run.state != RunState::AwaitingCi {
            continue;
        }

        let Some(pr_number) = run.pr_number else {
            tracing::warn!(
                run_id = %run.id,
                "run stuck in AwaitingCi with no pr_number recorded; nothing to resume watching, marking Failed"
            );
            run.state
                .validate_transition(RunState::Failed, run.total_steps)?;
            db::fail_run_with_pending_cleanup(&pool, &run.id).await?;
            reclaim_orphan_resources(&pool, &run).await?;
            delete_gate_staging_ref(&bare_repo_path, &run.id).await;
            resumed_run_ids.push(run.id);
            continue;
        };

        tracing::info!(run_id = %run.id, pr_number, "resuming CI watch for a run stuck in AwaitingCi");
        let runs_dir = warden_home.join("runs");
        let listener = CiResultListener::bind(&run.id, &runs_dir).await?;
        let gate_child = trigger
            .trigger_resume_watch(&run.id, pr_number, listener.socket_path())
            .await?;

        let outcome = orchestrator
            .await_and_apply_ci_result(&run.id, &listener, gate_child)
            .await?;
        if let PostConvergenceOutcome::Terminal(_) = &outcome {
            delete_gate_staging_ref(&bare_repo_path, &run.id).await;
        }
        resumed_run_ids.push(run.id);
    }

    Ok(resumed_run_ids)
}

/// Resumes quota-suspended convergence loops whose persisted reset time has elapsed.
pub async fn resume_quota_suspended_runs(pool: SqlitePool) -> Result<Vec<String>> {
    resume_quota_suspended_runs_at(pool, Utc::now().timestamp()).await
}

async fn resume_quota_suspended_runs_at(pool: SqlitePool, now_unix: i64) -> Result<Vec<String>> {
    let due_continuations = db::list_due_quota_continuations(&pool, now_unix).await?;
    let mut resumed_run_ids = Vec::new();

    for candidate in due_continuations {
        if !db::claim_due_quota_continuation(&pool, &candidate, now_unix).await? {
            continue;
        }
        let claimed_run_id = candidate.run_id.clone();
        match resume_claimed_quota_continuation(&pool, candidate).await {
            Ok(Some(run_id)) => resumed_run_ids.push(run_id),
            Ok(None) => {}
            Err(error) => {
                // The lease remains present until `insert_agent_process` commits.
                if let Err(cleanup_error) =
                    db::fail_quota_resume_claim(&pool, &claimed_run_id).await
                {
                    tracing::error!(run_id = %claimed_run_id, %cleanup_error, "failed to terminally release a failed quota resume claim");
                    return Err(cleanup_error);
                }
                return Err(error);
            }
        }
    }

    Ok(resumed_run_ids)
}

/// Completes one claim that this process acquired.
async fn resume_claimed_quota_continuation(
    pool: &SqlitePool,
    candidate: db::DueQuotaContinuation,
) -> Result<Option<String>> {
    let run_id = candidate.run_id;
    let Some(run) = db::get_run(pool, &run_id).await? else {
        tracing::warn!(run_id, "due quota continuation has no run row; skipping");
        return Ok(None);
    };

    if run.state != RunState::ResumingQuota {
        tracing::warn!(run_id = %run.id, state = run.state.as_str(), "claimed quota continuation no longer has its resuming state; skipping");
        return Ok(None);
    }

    let Some(record) = db::get_quota_continuation(pool, &run.id).await? else {
        fail_quota_continuation_recovery(
            pool,
            &run,
            "no quota continuation checkpoint was persisted",
        )
        .await?;
        return Ok(None);
    };

    let restored =
        match super::continuation::decode_run(&run.id, &record.config_json, &record.state_json) {
            Ok(restored) => restored,
            Err(error) => {
                fail_quota_continuation_recovery(pool, &run, error.to_string()).await?;
                return Ok(None);
            }
        };

    if let Err(reason) = validate_restored_run_config(&run, &restored.config) {
        fail_quota_continuation_recovery(pool, &run, reason).await?;
        return Ok(None);
    }

    let policy_rules = match crate::policy_config::parse_repo_policy(
        &restored.config.repo_path,
        restored.execution_context.policy_yaml.as_deref(),
    ) {
        Ok(rules) => rules,
        Err(error) => {
            fail_quota_continuation_recovery(pool, &run, error.to_string()).await?;
            return Ok(None);
        }
    };
    if restored.execution_context.approval == ApprovalConfig::InteractiveTty {
        tracing::warn!(
            run_id = %run.id,
            "original policy used a non-durable interactive approval channel; resumed \
             approvals will be denied fail-closed"
        );
    }
    let policy_gate = Arc::new(PolicyGate::new(warden_policy::Evaluator::new(policy_rules)));
    let hooks = match crate::hook_config::parse_repo_hooks(
        &restored.config.repo_path,
        restored.execution_context.hooks_toml.as_deref(),
        Arc::new(LocalSandbox::new()),
        Arc::clone(&policy_gate),
    ) {
        Ok(hooks) => hooks,
        Err(error) => {
            fail_quota_continuation_recovery(pool, &run, error.to_string()).await?;
            return Ok(None);
        }
    };
    let runner = restored.execution_context.tool;
    let sandbox = restored
        .execution_context
        .sandbox
        .build(&restored.config.repo_path);
    let orchestrator = Orchestrator::new(pool.clone())
        .with_sandbox(sandbox)
        .with_hooks(hooks)
        .with_policy_gate(policy_gate)
        .with_run_execution_context(restored.execution_context)
        .with_quota_anticipation_threshold(restored.quota_anticipation_threshold);
    let (_, final_state) = orchestrator
        .resume_convergence_loop(
            run.id.clone(),
            restored.config,
            &runner,
            CancellationToken::new(),
            restored.continuation,
        )
        .await?;

    if !matches!(final_state, RunState::AwaitingQuotaReset { .. }) {
        db::delete_quota_continuation(pool, &run.id).await?;
    }
    Ok(Some(run.id))
}

/// Binds checkpoint JSON back to the immutable values independently stored in `runs`.
fn validate_restored_run_config(
    run: &db::Run,
    config: &RunConfig,
) -> std::result::Result<(), String> {
    let mismatch = if config.repo_path != Path::new(&run.repo_path) {
        Some(format!(
            "repo_path {:?} does not match run row {:?}",
            config.repo_path, run.repo_path
        ))
    } else if config.branch != run.branch {
        Some(format!(
            "branch {:?} does not match run row {:?}",
            config.branch, run.branch
        ))
    } else if config.intent != run.intent {
        Some("intent does not match run row".to_string())
    } else if config.max_cycles != run.max_review_cycles {
        Some(format!(
            "max_cycles {} does not match run row {}",
            config.max_cycles, run.max_review_cycles
        ))
    } else if config.max_cycles != run.max_test_cycles {
        Some(format!(
            "max_cycles {} does not match run row {}",
            config.max_cycles, run.max_test_cycles
        ))
    } else if config.workflow.steps.len() != run.total_steps as usize {
        Some(format!(
            "workflow has {} steps but run row records {}",
            config.workflow.steps.len(),
            run.total_steps
        ))
    } else if config.max_cycles != run.max_extra_step_cycles {
        Some(format!(
            "max_cycles {} does not match run row {}",
            config.max_cycles, run.max_extra_step_cycles
        ))
    } else {
        None
    };

    mismatch.map_or(Ok(()), Err)
}

/// Makes an unusable quota checkpoint explicit and terminal.
async fn fail_quota_continuation_recovery(
    pool: &SqlitePool,
    run: &db::Run,
    reason: impl std::fmt::Display,
) -> Result<()> {
    tracing::error!(
        run_id = %run.id,
        %reason,
        "cannot resume quota-suspended run; marking Failed"
    );
    run.state
        .validate_transition(RunState::Failed, run.total_steps)?;
    if !db::fail_quota_resume_claim(pool, &run.id).await? {
        return Err(WardenError::InvalidQuotaContinuation {
            run_id: run.id.clone(),
            reason: "quota resume claim disappeared before it could be failed".to_string(),
        });
    }
    reclaim_orphan_resources(pool, run).await?;
    Ok(())
}

/// Reclaims both kinds of resources a crashed run may have left orphaned.
async fn reclaim_orphan_resources(pool: &SqlitePool, run: &db::Run) -> Result<()> {
    reclaim_orphan_resources_with(pool, run, reclaim_run_containers).await
}

async fn reclaim_orphan_resources_with<F, Fut>(
    pool: &SqlitePool,
    run: &db::Run,
    reclaim_containers: F,
) -> Result<()>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = warden_sandbox::Result<usize>>,
{
    db::mark_run_cleanup_pending(pool, &run.id).await?;
    let processes_clean = terminate_orphan_processes(pool, &run.id).await?;
    let containers_clean = match reclaim_containers(run.id.clone()).await {
        Ok(0) => true,
        Ok(count) => {
            tracing::warn!(run_id = %run.id, count, "removed orphan Docker containers during crash recovery");
            true
        }
        Err(error) => {
            tracing::error!(run_id = %run.id, %error, "failed to remove orphan Docker containers during crash recovery");
            false
        }
    };
    let worktrees_clean = cleanup_orphan_worktrees(pool, run).await?;
    if processes_clean && containers_clean && worktrees_clean {
        db::clear_run_cleanup_pending(pool, &run.id).await?;
    } else {
        tracing::warn!(run_id = %run.id, "orphan cleanup remains pending for a later recovery pass");
    }
    Ok(())
}

async fn cleanup_orphan_worktrees(pool: &SqlitePool, run: &db::Run) -> Result<bool> {
    let entries = db::list_cycle_worktree_entries_for_run(pool, &run.id).await?;
    if entries.is_empty() {
        return Ok(true);
    }

    let main_repo_path = Path::new(&run.repo_path);
    let mut complete = true;
    let mut removed_entries = Vec::new();
    for entry in &entries {
        match worktree::remove_orphan_worktree(main_repo_path, Path::new(&entry.path)).await {
            Ok(()) => removed_entries.push(entry),
            Err(error) => {
                tracing::error!(run_id = %run.id, worktree_path = %entry.path, %error, "failed to remove orphaned worktree");
                complete = false;
            }
        }
    }

    if let Err(error) = worktree::prune_worktrees(main_repo_path).await {
        tracing::error!(run_id = %run.id, %error, "git worktree prune failed during crash recovery");
        complete = false;
    } else {
        for entry in removed_entries {
            if let Err(error) =
                db::clear_cycle_worktree_path(pool, &entry.cycle_id, &entry.role).await
            {
                tracing::error!(run_id = %run.id, cycle_id = %entry.cycle_id, %error, "failed to clear recorded worktree path after removing it");
                complete = false;
            }
        }
    }

    Ok(complete)
}

const RECOVERY_TERMINATED_EXIT_CODE: i32 = -1;

async fn terminate_orphan_processes(pool: &SqlitePool, run_id: &str) -> Result<bool> {
    let open_processes = db::list_open_agent_processes_for_run(pool, run_id).await?;
    let mut complete = true;

    for open_process in open_processes {
        if let Err(error) = process::kill_pid(open_process.pid, open_process.pid_started_at_unix) {
            tracing::error!(
                run_id,
                pid = open_process.pid,
                %error,
                "failed to terminate a live orphan agent process; leaving its row open for a later recovery pass"
            );
            complete = false;
            continue;
        }

        if let Err(error) =
            db::mark_agent_process_ended(pool, &open_process.id, RECOVERY_TERMINATED_EXIT_CODE)
                .await
        {
            tracing::error!(
                run_id,
                pid = open_process.pid,
                %error,
                "failed to mark a terminated orphan agent process ended"
            );
            complete = false;
        }
    }

    Ok(complete)
}
