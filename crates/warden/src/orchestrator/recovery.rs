use super::gate_tail::delete_gate_staging_ref;
use super::*;

/// Crash recovery (Architecture.md §6 "Règle de récupération" / §9 Disaster Recovery).
pub async fn recover_crashed_runs(pool: &SqlitePool) -> Result<Vec<String>> {
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
        db::update_run_state(pool, &run.id, RunState::Failed).await?;
        db::delete_quota_continuation(pool, &run.id).await?;
        tracing::warn!(run_id = %run.id, previous_state = run.state.as_str(), "run recovered as Failed: no live process found");

        reclaim_orphan_resources(pool, &run).await;
        failed_run_ids.push(run.id);
    }

    let failed_runs_needing_cleanup = db::list_failed_runs_with_pending_cleanup(pool).await?;
    for run in failed_runs_needing_cleanup {
        tracing::warn!(run_id = %run.id, "resuming orphan cleanup for a run already marked Failed by an earlier, interrupted recovery pass");
        reclaim_orphan_resources(pool, &run).await;
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
            db::update_run_state(&pool, &run.id, RunState::Failed).await?;
            reclaim_orphan_resources(&pool, &run).await;
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

    if let Some(active_cycle) = restored.continuation.active_cycle.as_ref() {
        if !db::cycle_belongs_to_run(pool, &active_cycle.cycle_id, &run.id).await? {
            fail_quota_continuation_recovery(
                pool,
                &run,
                format!(
                    "checkpoint active cycle {} does not belong to this run",
                    active_cycle.cycle_id
                ),
            )
            .await?;
            return Ok(None);
        }
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
    } else if config.max_review_cycles != run.max_review_cycles {
        Some(format!(
            "max_review_cycles {} does not match run row {}",
            config.max_review_cycles, run.max_review_cycles
        ))
    } else if config.max_test_cycles != run.max_test_cycles {
        Some(format!(
            "max_test_cycles {} does not match run row {}",
            config.max_test_cycles, run.max_test_cycles
        ))
    } else if config.workflow.steps.len() != run.total_steps as usize {
        Some(format!(
            "workflow has {} steps but run row records {}",
            config.workflow.steps.len(),
            run.total_steps
        ))
    } else if config.max_extra_step_cycles != run.max_extra_step_cycles {
        Some(format!(
            "max_extra_step_cycles {} does not match run row {}",
            config.max_extra_step_cycles, run.max_extra_step_cycles
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
    reclaim_orphan_resources(pool, run).await;
    Ok(())
}

/// Reclaims both kinds of resources a crashed run may have left orphaned.
async fn reclaim_orphan_resources(pool: &SqlitePool, run: &db::Run) {
    if let Err(error) = terminate_orphan_processes(pool, &run.id).await {
        tracing::error!(run_id = %run.id, %error, "failed to terminate orphan agent processes during crash recovery");
    }
    if let Err(error) = cleanup_orphan_worktrees(pool, run).await {
        tracing::error!(run_id = %run.id, %error, "failed to clean up orphaned worktrees during crash recovery");
    }
}

async fn cleanup_orphan_worktrees(pool: &SqlitePool, run: &db::Run) -> Result<()> {
    let entries = db::list_cycle_worktree_entries_for_run(pool, &run.id).await?;
    if entries.is_empty() {
        return Ok(());
    }

    let main_repo_path = Path::new(&run.repo_path);
    for entry in &entries {
        match worktree::remove_orphan_worktree(main_repo_path, Path::new(&entry.path)).await {
            Ok(()) => {
                if let Err(error) =
                    db::clear_cycle_worktree_path(pool, &entry.cycle_id, &entry.role).await
                {
                    tracing::error!(run_id = %run.id, cycle_id = %entry.cycle_id, %error, "failed to clear recorded worktree path after removing it");
                }
            }
            Err(error) => {
                tracing::error!(run_id = %run.id, worktree_path = %entry.path, %error, "failed to remove orphaned worktree");
            }
        }
    }

    if let Err(error) = worktree::prune_worktrees(main_repo_path).await {
        tracing::error!(run_id = %run.id, %error, "git worktree prune failed during crash recovery");
    }

    Ok(())
}

const RECOVERY_TERMINATED_EXIT_CODE: i32 = -1;

async fn terminate_orphan_processes(pool: &SqlitePool, run_id: &str) -> Result<()> {
    let open_processes = db::list_open_agent_processes_for_run(pool, run_id).await?;

    for open_process in open_processes {
        if let Err(error) = process::kill_pid(open_process.pid, open_process.pid_started_at_unix) {
            tracing::error!(
                run_id,
                pid = open_process.pid,
                %error,
                "failed to terminate a live orphan agent process; leaving its row open for a later recovery pass"
            );
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
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::test_support::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn corrupt_quota_checkpoint_fails_closed_and_is_deleted() {
        const RESET_BOUNDARY: i64 = 1_800_000_000;
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        db::insert_run(
            &pool,
            "corrupt-quota-run",
            "/tmp/repo",
            "main",
            "quota recovery",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::update_run_state(&pool, "corrupt-quota-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::suspend_run_with_quota_continuation(
            &pool,
            "corrupt-quota-run",
            RESET_BOUNDARY,
            "{not valid JSON",
            "{}",
        )
        .await
        .unwrap();

        assert!(recover_crashed_runs(&pool).await.unwrap().is_empty());
        assert_eq!(
            db::get_run(&pool, "corrupt-quota-run")
                .await
                .unwrap()
                .unwrap()
                .state,
            RunState::AwaitingQuotaReset {
                resets_at: RESET_BOUNDARY,
            }
        );

        let resumed = resume_quota_suspended_runs_at(pool.clone(), RESET_BOUNDARY)
            .await
            .unwrap();

        assert!(resumed.is_empty());
        assert_eq!(
            db::get_run(&pool, "corrupt-quota-run")
                .await
                .unwrap()
                .unwrap()
                .state,
            RunState::Failed
        );
        assert!(db::get_quota_continuation(&pool, "corrupt-quota-run")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn missing_quota_checkpoint_fails_closed() {
        const RESET_BOUNDARY: i64 = 1_800_000_000;
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        db::insert_run(
            &pool,
            "missing-quota-checkpoint-run",
            "/tmp/repo",
            "main",
            "quota recovery",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::update_run_state(
            &pool,
            "missing-quota-checkpoint-run",
            RunState::AwaitingQuotaReset {
                resets_at: RESET_BOUNDARY,
            },
        )
        .await
        .unwrap();

        let resumed = resume_quota_suspended_runs_at(pool.clone(), RESET_BOUNDARY)
            .await
            .unwrap();

        assert!(resumed.is_empty());
        assert_eq!(
            db::get_run(&pool, "missing-quota-checkpoint-run")
                .await
                .unwrap()
                .unwrap()
                .state,
            RunState::Failed
        );
    }

    #[tokio::test]
    async fn failed_quota_resume_task_with_live_owner_terminally_releases_its_claim() {
        const RESET_BOUNDARY: i64 = 1_800_000_000;
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "resume must not strand its claim".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(always_passing_tester()),
                definition(always_passing_tester()),
                definition(always_passing_tester()),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        db::insert_run(
            &pool,
            "failed-live-owner-quota-run",
            &config.repo_path.display().to_string(),
            &config.branch,
            &config.intent,
            config.max_review_cycles,
            config.max_test_cycles,
            config.workflow.steps.len() as u32,
            config.max_extra_step_cycles,
        )
        .await
        .unwrap();
        db::update_run_state(&pool, "failed-live-owner-quota-run", RunState::CoderRunning)
            .await
            .unwrap();

        let execution_context = RunExecutionContext {
            tool: crate::tool_adapter::ToolName::Claude,
            sandbox: SandboxConfig::Worktree,
            hooks_toml: None,
            policy_yaml: None,
            approval: ApprovalConfig::FailClosed,
        };
        let config_json =
            super::continuation::encode_run_config(&config, &execution_context, 0.9).unwrap();
        let state_json = super::continuation::encode_convergence_state(
            &ConvergenceContinuation::new("not-a-real-git-commit".to_string(), 3),
        )
        .unwrap();
        db::suspend_run_with_quota_continuation(
            &pool,
            "failed-live-owner-quota-run",
            RESET_BOUNDARY,
            &config_json,
            &state_json,
        )
        .await
        .unwrap();

        assert!(resume_quota_suspended_runs_at(pool.clone(), RESET_BOUNDARY)
            .await
            .is_err());
        assert!(process::is_process_alive(
            std::process::id(),
            process::process_start_time(std::process::id()).unwrap()
        ));
        assert_eq!(
            db::get_run(&pool, "failed-live-owner-quota-run")
                .await
                .unwrap()
                .unwrap()
                .state,
            RunState::Failed
        );
        assert!(
            db::get_quota_resume_lease(&pool, "failed-live-owner-quota-run")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db::get_quota_continuation(&pool, "failed-live-owner-quota-run")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn recovery_preserves_live_quota_lease_after_restored_transition_before_agent_handoff() {
        const RESET_BOUNDARY: i64 = 1_800_000_000;
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        db::insert_run(
            &pool,
            "live-quota-resume-run",
            "/tmp/repo",
            "main",
            "quota recovery",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::update_run_state(&pool, "live-quota-resume-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::suspend_run_with_quota_continuation(
            &pool,
            "live-quota-resume-run",
            RESET_BOUNDARY,
            r#"{"config":"checkpoint"}"#,
            r#"{"state":"checkpoint"}"#,
        )
        .await
        .unwrap();

        let candidate = db::list_due_quota_continuations(&pool, RESET_BOUNDARY)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            db::claim_due_quota_continuation(&pool, &candidate, RESET_BOUNDARY)
                .await
                .unwrap()
        );

        assert!(recover_crashed_runs(&pool).await.unwrap().is_empty());
        let claimed_run = db::get_run(&pool, "live-quota-resume-run")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed_run.state, RunState::ResumingQuota);
        let lease = db::get_quota_resume_lease(&pool, "live-quota-resume-run")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lease.owner_pid, std::process::id());
        assert!(process::is_process_alive(
            lease.owner_pid,
            lease.owner_started_at_unix
        ));

        claimed_run
            .state
            .validate_transition(RunState::CoderRunning, claimed_run.total_steps)
            .unwrap();
        db::update_run_state(&pool, "live-quota-resume-run", RunState::CoderRunning)
            .await
            .unwrap();
        assert!(
            recover_crashed_runs(&pool).await.unwrap().is_empty(),
            "the deterministic interleaving after the restored transition but before the \
             agent-process write must retain the live claim"
        );
        assert_eq!(
            db::get_run(&pool, "live-quota-resume-run")
                .await
                .unwrap()
                .unwrap()
                .state,
            RunState::CoderRunning
        );
        assert!(db::get_quota_resume_lease(&pool, "live-quota-resume-run")
            .await
            .unwrap()
            .is_some());

        db::insert_cycle(&pool, "live-quota-resume-cycle", "live-quota-resume-run", 1)
            .await
            .unwrap();
        let mut agent = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let agent_pid = agent.id().unwrap();
        db::insert_agent_process(
            &pool,
            "live-quota-resume-process",
            "live-quota-resume-cycle",
            "coder",
            agent_pid,
            "/tmp/wt",
        )
        .await
        .unwrap();
        assert!(db::get_quota_resume_lease(&pool, "live-quota-resume-run")
            .await
            .unwrap()
            .is_none());
        agent.kill().await.unwrap();
    }

    #[tokio::test]
    async fn recovery_marks_intermediate_run_failed_when_its_process_is_dead() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(
            &pool,
            "crashed-run",
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
        db::update_run_state(&pool, "crashed-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::insert_cycle(&pool, "crashed-cycle", "crashed-run", 1)
            .await
            .unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id().unwrap();
        child.wait().await.unwrap();

        db::insert_agent_process(
            &pool,
            "crashed-process",
            "crashed-cycle",
            "coder",
            dead_pid,
            "/tmp/wt",
        )
        .await
        .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert_eq!(failed, vec!["crashed-run".to_string()]);

        let run = db::get_run(&pool, "crashed-run").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    #[tokio::test]
    async fn recovery_leaves_intermediate_run_alone_when_its_process_is_alive() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(&pool, "live-run", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
            .await
            .unwrap();
        db::update_run_state(&pool, "live-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::insert_cycle(&pool, "live-cycle", "live-run", 1)
            .await
            .unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .unwrap();
        let live_pid = child.id().unwrap();

        db::insert_agent_process(
            &pool,
            "live-process",
            "live-cycle",
            "coder",
            live_pid,
            "/tmp/wt",
        )
        .await
        .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert!(failed.is_empty());

        let run = db::get_run(&pool, "live-run").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::CoderRunning);

        child.kill().await.unwrap();
    }

    #[tokio::test]
    async fn recovery_removes_an_orphaned_worktree_left_behind_by_a_crashed_run() {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();
        let worktree = worktree_manager
            .create("orphan-recovery-run", "coder", "HEAD")
            .await
            .unwrap();
        let worktree_path = worktree.path().to_path_buf();
        std::mem::forget(worktree);
        assert!(worktree_path.exists(), "precondition: worktree exists");

        db::insert_run(
            &pool,
            "orphan-recovery-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::update_run_state(&pool, "orphan-recovery-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::insert_cycle(&pool, "orphan-recovery-cycle", "orphan-recovery-run", 1)
            .await
            .unwrap();
        db::set_cycle_worktree_path(
            &pool,
            "orphan-recovery-cycle",
            "coder",
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id().unwrap();
        child.wait().await.unwrap();
        db::insert_agent_process(
            &pool,
            "orphan-recovery-process",
            "orphan-recovery-cycle",
            "coder",
            dead_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert_eq!(failed, vec!["orphan-recovery-run".to_string()]);

        assert!(
            !worktree_path.exists(),
            "orphaned worktree must be removed by crash recovery"
        );
    }

    #[tokio::test]
    async fn recovery_terminates_an_orphaned_live_agent_process() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(
            &pool,
            "orphan-process-run",
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
        db::update_run_state(&pool, "orphan-process-run", RunState::RunningStep(2))
            .await
            .unwrap();
        db::insert_cycle(&pool, "orphan-process-cycle", "orphan-process-run", 1)
            .await
            .unwrap();

        let mut live_child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let live_pid = live_child.id().unwrap();
        db::insert_agent_process(
            &pool,
            "orphan-process-live",
            "orphan-process-cycle",
            "tester",
            live_pid,
            "/tmp/wt/tester",
        )
        .await
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let mut dead_child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = dead_child.id().unwrap();
        dead_child.wait().await.unwrap();
        db::insert_agent_process(
            &pool,
            "orphan-process-dead",
            "orphan-process-cycle",
            "reviewer",
            dead_pid,
            "/tmp/wt/reviewer",
        )
        .await
        .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert_eq!(failed, vec!["orphan-process-run".to_string()]);

        let exit_status = live_child.wait().await.unwrap();
        assert!(
            !exit_status.success(),
            "orphaned live process must have been killed by recovery"
        );

        let open_processes = db::list_open_agent_processes_for_run(&pool, "orphan-process-run")
            .await
            .unwrap();
        assert!(
            open_processes.is_empty(),
            "every agent_processes row for a Failed run must be marked ended by recovery"
        );
    }

    #[tokio::test]
    async fn recovery_never_kills_a_live_process_whose_pid_fingerprint_no_longer_matches() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(
            &pool,
            "pid-reuse-run",
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
        db::update_run_state(&pool, "pid-reuse-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::insert_cycle(&pool, "pid-reuse-cycle", "pid-reuse-run", 1)
            .await
            .unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let real_start_time = process::process_start_time(pid).unwrap();
        let bogus_start_time = real_start_time + 1_000_000;

        sqlx::query!(
                "INSERT INTO agent_processes (id, cycle_id, role, pid, pid_started_at_unix, worktree_path, started_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                "pid-reuse-process",
                "pid-reuse-cycle",
                "coder",
                pid,
                bogus_start_time,
                "/tmp/wt",
                "2020-01-01T00:00:00+00:00",
            )
            .execute(&pool)
            .await
            .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert_eq!(failed, vec!["pid-reuse-run".to_string()]);

        assert!(
            process::is_process_alive(pid, real_start_time),
            "a process whose PID was reused must never be killed by crash recovery"
        );

        let open_processes = db::list_open_agent_processes_for_run(&pool, "pid-reuse-run")
            .await
            .unwrap();
        assert!(
                open_processes.is_empty(),
                "the stale agent_processes row must be marked ended even though its process was never touched"
            );

        child.kill().await.unwrap();
    }

    #[tokio::test]
    async fn recovery_cleans_up_orphans_even_for_a_run_already_marked_failed_by_an_earlier_crashed_recovery_pass(
    ) {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();
        let worktree = worktree_manager
            .create("crash-during-recovery-run", "coder", "HEAD")
            .await
            .unwrap();
        let worktree_path = worktree.path().to_path_buf();
        std::mem::forget(worktree);
        assert!(
            worktree_path.exists(),
            "precondition: orphan worktree exists"
        );

        db::insert_run(
            &pool,
            "crash-during-recovery-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(
            &pool,
            "crash-during-recovery-cycle",
            "crash-during-recovery-run",
            1,
        )
        .await
        .unwrap();
        db::set_cycle_worktree_path(
            &pool,
            "crash-during-recovery-cycle",
            "coder",
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        db::insert_agent_process(
            &pool,
            "crash-during-recovery-process",
            "crash-during-recovery-cycle",
            "coder",
            pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        db::update_run_state(&pool, "crash-during-recovery-run", RunState::Failed)
            .await
            .unwrap();

        recover_crashed_runs(&pool).await.unwrap();

        let _ = child.kill().await;
        let _ = child.wait().await;

        assert!(
            !worktree_path.exists(),
            "BUG: a run already marked Failed by an interrupted recovery pass is never \
                 revisited by list_intermediate_runs, so its orphan worktree is leaked forever, \
                 not just cleaned up late"
        );

        let open_processes =
            db::list_open_agent_processes_for_run(&pool, "crash-during-recovery-run")
                .await
                .unwrap();
        assert!(
            open_processes.is_empty(),
            "BUG: a run already marked Failed by an interrupted recovery pass leaves its \
                 agent_processes row open forever, and the process itself keeps running"
        );
    }

    #[tokio::test]
    async fn second_recovery_pass_is_a_noop_once_a_failed_runs_cleanup_has_actually_succeeded() {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();
        let worktree = worktree_manager
            .create("idempotent-recovery-run", "coder", "HEAD")
            .await
            .unwrap();
        let worktree_path = worktree.path().to_path_buf();
        std::mem::forget(worktree);

        db::insert_run(
            &pool,
            "idempotent-recovery-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::update_run_state(&pool, "idempotent-recovery-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::insert_cycle(
            &pool,
            "idempotent-recovery-cycle",
            "idempotent-recovery-run",
            1,
        )
        .await
        .unwrap();
        db::set_cycle_worktree_path(
            &pool,
            "idempotent-recovery-cycle",
            "coder",
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id().unwrap();
        child.wait().await.unwrap();
        db::insert_agent_process(
            &pool,
            "idempotent-recovery-process",
            "idempotent-recovery-cycle",
            "coder",
            dead_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert_eq!(failed, vec!["idempotent-recovery-run".to_string()]);
        assert!(
            !worktree_path.exists(),
            "precondition: first pass must actually remove the worktree"
        );

        let pending_after_first_pass = db::list_failed_runs_with_pending_cleanup(&pool)
            .await
            .unwrap();
        assert!(
            pending_after_first_pass.is_empty(),
            "precondition: first pass must leave nothing pending"
        );

        let failed_again = recover_crashed_runs(&pool).await.unwrap();
        assert!(
            failed_again.is_empty(),
            "a run already Failed with nothing pending must not be reported as newly \
                 failed again"
        );

        let run = db::get_run(&pool, "idempotent-recovery-run")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.state, RunState::Failed);
    }
}
