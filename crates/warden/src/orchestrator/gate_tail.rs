//! /'s post-`Converged` tail: push into the local bare gate repo, trigger `warden-gated`'s `run-
//! tail`, and await its terminal CI result.

use super::*;

const GATE_CHILD_GRACE_PERIOD: Duration = Duration::from_secs(2);

/// The ref prefix `warden` stages a converged run's commit under in the local bare gate repo.
const GATE_STAGING_REF_PREFIX: &str = "refs/warden-staging/";

async fn evidence_rows_for_run(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Vec<warden_core::EvidenceRow>> {
    let rows = db::list_evidence_for_run(pool, run_id).await?;
    Ok(rows
        .into_iter()
        .map(|row| warden_core::EvidenceRow {
            cycle_number: row.cycle_number,
            evidence_type: row.evidence.evidence_type,
            repo_relative_path: row.evidence.file_path,
            description: row.evidence.description,
        })
        .collect())
}

async fn push_converged_commit_to_bare_repo(
    repo_path: &Path,
    bare_repo_path: &Path,
    commit_sha: &str,
    run_id: &str,
) -> Result<()> {
    let refspec = format!("{commit_sha}:{GATE_STAGING_REF_PREFIX}{run_id}");
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(NO_HOST_HOOKS)
        .args(["push", "--force"])
        .arg(bare_repo_path)
        .arg(&refspec)
        .output()
        .await?;

    if !output.status.success() {
        return Err(WardenError::Worktree(WorktreeError::GitCommandFailed {
            command: format!(
                "git -C {} push --force {} {refspec}",
                repo_path.display(),
                bare_repo_path.display()
            ),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }
    Ok(())
}

/// Waits for `warden-gated`'s single terminal CI message on `listener`, bounded by `gate_child`'s
/// liveness rather than any wall-clock timeout.
async fn await_ci_result(
    run_id: &str,
    listener: &CiResultListener,
    gate_child: GateChild,
) -> Result<CiResultMessage> {
    tokio::select! {
        biased;
        result = listener.receive_no_timeout() => result,
        () = wait_child_then_grace(gate_child) => {
            Err(WardenError::GateChildDiedWithoutResult {
                run_id: run_id.to_string(),
            })
        }
    }
}

/// Resolves once the triggered child has exited *and* [`GATE_CHILD_GRACE_PERIOD`] has since
/// elapsed.
async fn wait_child_then_grace(gate_child: GateChild) {
    gate_child.wait_exit().await;
    tokio::time::sleep(GATE_CHILD_GRACE_PERIOD).await;
}

/// Best-effort removal of a run's staging ref from the bare gate repo once the run is terminal.
pub(super) async fn delete_gate_staging_ref(bare_repo_path: &Path, run_id: &str) {
    let ref_name = format!("{GATE_STAGING_REF_PREFIX}{run_id}");
    let result = tokio::process::Command::new("git")
        .arg("-C")
        .arg(bare_repo_path)
        .args(["update-ref", "-d", &ref_name])
        .output()
        .await;
    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::debug!(
            run_id,
            stderr = %String::from_utf8_lossy(&output.stderr),
            "failed to delete the gate staging ref on a terminal outcome (best-effort)"
        ),
        Err(error) => tracing::debug!(
            run_id,
            %error,
            "failed to run git to delete the gate staging ref (best-effort)"
        ),
    }
}

pub(super) async fn protect_cycle_commit(
    main_repo_path: &Path,
    run_id: &str,
    cycle_number: u32,
    commit_sha: &str,
) -> Result<()> {
    let ref_name = format!("refs/warden/runs/{run_id}/cycle-{cycle_number}");
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(main_repo_path)
        .args(NO_HOST_HOOKS)
        .args(["update-ref", &ref_name, commit_sha])
        .output()
        .await?;

    if !output.status.success() {
        return Err(WardenError::Worktree(WorktreeError::GitCommandFailed {
            command: format!(
                "git -C {} update-ref {ref_name} {commit_sha}",
                main_repo_path.display()
            ),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }

    Ok(())
}

impl Orchestrator {
    pub(super) async fn drive_post_convergence_tail<G: GateTrigger>(
        &self,
        run_id: &str,
        config: &RunConfig,
        converged_commit: &str,
        trigger: &G,
    ) -> Result<PostConvergenceOutcome> {
        let Some(gate_config) = &config.gate else {
            unreachable!("drive_post_convergence_tail is only called when config.gate is Some");
        };

        let existing_pr_number = db::get_run(&self.pool, run_id)
            .await?
            .and_then(|run| run.pr_number);
        let evidence = evidence_rows_for_run(&self.pool, run_id).await?;

        // / the policy decision layer's one wiring into this crate's push path.
        if let PolicyOutcome::Blocked { reason } = self
            .policy_gate
            .decide(
                run_id,
                &format!("git_push to branch {:?}", config.branch),
                &warden_policy::Action::GitPush {
                    branch: config.branch.clone(),
                },
            )
            .await
        {
            tracing::warn!(run_id, reason, "policy blocked the push; failing the run");
            self.transition(run_id, RunState::Failed).await?;
            delete_gate_staging_ref(&gate_config.bare_repo_path, run_id).await;
            return Ok(PostConvergenceOutcome::Terminal(RunState::Failed));
        }

        match self.transition(run_id, RunState::Pushed).await? {
            TransitionEffect::Continue => {}
            TransitionEffect::Blocked { point, reason } => {
                self.fail_run_on_block(run_id, point, &reason).await?;
                delete_gate_staging_ref(&gate_config.bare_repo_path, run_id).await;
                return Ok(PostConvergenceOutcome::Terminal(RunState::Failed));
            }
            TransitionEffect::FindingsEmitted { point, findings } => {
                self.record_unrouted_findings(point, &findings).await?;
            }
        }
        push_converged_commit_to_bare_repo(
            &config.repo_path,
            &gate_config.bare_repo_path,
            converged_commit,
            run_id,
        )
        .await?;

        let runs_dir = config.warden_home.join("runs");
        let listener = CiResultListener::bind(run_id, &runs_dir).await?;

        let branch = format!("warden/{run_id}");
        let summary_body = format!(
            "Run {run_id} converged.\n\nIntent:\n{}\n",
            config.intent.trim()
        );
        let gate_child = trigger
            .trigger_run_tail(&RunTailTrigger {
                run_id,
                branch: &branch,
                base_branch: &config.branch,
                intent: &config.intent,
                pushed_commit_sha: converged_commit,
                summary_body: &summary_body,
                ci_result_socket: listener.socket_path(),
                evidence: &evidence,
                existing_pr_number,
            })
            .await?;

        self.transition(run_id, RunState::AwaitingCi).await?;

        let outcome = self
            .await_and_apply_ci_result(run_id, &listener, gate_child)
            .await?;

        if let PostConvergenceOutcome::Terminal(_) = &outcome {
            delete_gate_staging_ref(&gate_config.bare_repo_path, run_id).await;
        }
        Ok(outcome)
    }

    /// Waits for `warden-gated`'s one terminal CI message and applies it, bounding the wait by the
    /// triggered child's *liveness* rather than a wall-clock timeout.
    pub(super) async fn await_and_apply_ci_result(
        &self,
        run_id: &str,
        listener: &CiResultListener,
        gate_child: GateChild,
    ) -> Result<PostConvergenceOutcome> {
        match await_ci_result(run_id, listener, gate_child).await {
            Ok(message) => self.apply_ci_result_message(run_id, &message).await,
            Err(WardenError::GateChildDiedWithoutResult { .. }) => {
                tracing::error!(
                    run_id,
                    "warden-gated exited without delivering a terminal CI result; failing the run"
                );
                self.fail_awaiting_ci_run(run_id).await
            }
            Err(error) => Err(error),
        }
    }

    async fn fail_awaiting_ci_run(&self, run_id: &str) -> Result<PostConvergenceOutcome> {
        let run =
            db::get_run(&self.pool, run_id)
                .await?
                .ok_or_else(|| WardenError::RunNotFound {
                    run_id: run_id.to_string(),
                })?;
        if run.state != RunState::AwaitingCi {
            return Ok(PostConvergenceOutcome::Terminal(run.state));
        }
        self.transition(run_id, RunState::Failed).await?;
        Ok(PostConvergenceOutcome::Terminal(RunState::Failed))
    }

    async fn apply_ci_result_message(
        &self,
        run_id: &str,
        message: &CiResultMessage,
    ) -> Result<PostConvergenceOutcome> {
        if message.run_id != run_id {
            return Err(WardenError::CiResultRunIdMismatch {
                expected: run_id.to_string(),
                actual: message.run_id.clone(),
            });
        }

        let run =
            db::get_run(&self.pool, run_id)
                .await?
                .ok_or_else(|| WardenError::RunNotFound {
                    run_id: run_id.to_string(),
                })?;

        if run.state != RunState::AwaitingCi {
            tracing::info!(
                run_id,
                ?run.state,
                "ignoring CI result: run already left AwaitingCi (stale/duplicate delivery)"
            );
            return Ok(PostConvergenceOutcome::Terminal(run.state));
        }

        if let Some(pr_number) = message.pr_number {
            db::set_run_pr_number(&self.pool, run_id, pr_number).await?;
        }

        let next_state = match message.outcome.as_ci_outcome() {
            Some(CiOutcome::ChecksFailed) => {
                let charged = run.current_review_cycle + 1;
                db::set_run_current_cycle(&self.pool, run_id, charged).await?;
                decide_next_state_after_ci(
                    CiOutcome::ChecksFailed,
                    db::get_run_workflow_entry(&self.pool, run_id).await?,
                    charged,
                    run.max_review_cycles,
                )
            }
            Some(ci_outcome) => decide_next_state_after_ci(
                ci_outcome,
                db::get_run_workflow_entry(&self.pool, run_id).await?,
                run.current_review_cycle,
                run.max_review_cycles,
            ),
            None => RunState::Failed,
        };
        match self.transition(run_id, next_state).await? {
            TransitionEffect::Continue => {}
            TransitionEffect::Blocked { point, reason } => {
                self.fail_run_on_block(run_id, point, &reason).await?;
                return Ok(PostConvergenceOutcome::Terminal(RunState::Failed));
            }
            TransitionEffect::FindingsEmitted { point, findings } => {
                self.record_unrouted_findings(point, &findings).await?;
            }
        }

        match next_state {
            RunState::RunningStep(_) => Ok(PostConvergenceOutcome::Reboucle {
                findings: message.outcome.findings()?,
            }),
            terminal => Ok(PostConvergenceOutcome::Terminal(terminal)),
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use warden_core::{CiWatchOutcome, HookPoint};

    use super::super::lifecycle_hook_tests::{single_command_step_workflow, BlockingHook};
    use super::*;
    use crate::hook::HookRegistry;

    /// A [`GateTrigger`] that panics if ever called -- proves a `before_push` block short-circuits
    /// before `warden-gated` would be triggered at all.
    struct UnreachableGateTrigger;

    impl GateTrigger for UnreachableGateTrigger {
        async fn trigger_run_tail(&self, _request: &RunTailTrigger<'_>) -> Result<GateChild> {
            panic!("a before_push block must short-circuit before triggering warden-gated");
        }

        async fn trigger_resume_watch(
            &self,
            _run_id: &str,
            _pr_number: u64,
            _ci_result_socket: &Path,
        ) -> Result<GateChild> {
            panic!("a before_push block must short-circuit before triggering warden-gated");
        }
    }

    fn registry_blocking(point: HookPoint) -> HookRegistry {
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(BlockingHook(point)));
        registry
    }

    #[tokio::test]
    async fn before_push_block_fails_the_run_before_pushing_anything() {
        let dir = TempDir::new().unwrap();
        let pool = db::connect(&dir.path().join("state.db")).await.unwrap();
        let run_id = "run-1";
        db::insert_run(
            &pool,
            run_id,
            "/tmp/does-not-matter",
            "main",
            "intent",
            3,
            3,
            1,
            3,
        )
        .await
        .unwrap();
        db::update_run_state(&pool, run_id, RunState::RunningStep(0))
            .await
            .unwrap();
        db::update_run_state(&pool, run_id, RunState::Converged)
            .await
            .unwrap();

        let orchestrator =
            Orchestrator::new(pool.clone()).with_hooks(registry_blocking(HookPoint::BeforePush));

        let config = RunConfig {
            repo_path: dir.path().to_path_buf(),
            warden_home: dir.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "test".to_string(),
            max_cycles: 3,
            workflow: single_command_step_workflow(),
            step_agents: Vec::new(),
            repository_agent_definitions: false,
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: Some(GateConfig {
                bare_repo_path: dir.path().join("bare.git"),
                gated_bin: dir.path().join("does-not-exist"),
                repo_slug: None,
                poll_interval_secs: 1,
                inactivity_timeout_secs: 1,
            }),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(run_id, &config, "deadbeef", &UnreachableGateTrigger)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Failed)
        ));
        let run = db::get_run(&pool, run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    /// The CI-reboucle path: `apply_ci_result_message` transitions back into `RunningStep`, firing
    /// `BeforeStep` exactly like the fresh-run and quota-resume paths -- a future refactor could
    /// silently un-enforce this one too (the same failure mode #106 fixed), so it gets its own test.
    #[tokio::test]
    async fn ci_checks_failed_reboucle_before_step_block_fails_the_run() {
        let dir = TempDir::new().unwrap();
        let pool = db::connect(&dir.path().join("state.db")).await.unwrap();
        let run_id = "run-1";
        db::insert_run(
            &pool,
            run_id,
            "/tmp/does-not-matter",
            "main",
            "intent",
            3,
            3,
            1,
            3,
        )
        .await
        .unwrap();
        db::set_run_workflow_entry(&pool, run_id, 0).await.unwrap();
        db::update_run_state(&pool, run_id, RunState::RunningStep(0))
            .await
            .unwrap();
        db::update_run_state(&pool, run_id, RunState::Converged)
            .await
            .unwrap();
        db::update_run_state(&pool, run_id, RunState::Pushed)
            .await
            .unwrap();
        db::update_run_state(&pool, run_id, RunState::AwaitingCi)
            .await
            .unwrap();

        let orchestrator =
            Orchestrator::new(pool.clone()).with_hooks(registry_blocking(HookPoint::BeforeStep));
        let message = CiResultMessage {
            run_id: run_id.to_string(),
            pr_number: None,
            outcome: CiWatchOutcome::checks_failed(&[]),
        };

        let outcome = orchestrator
            .apply_ci_result_message(run_id, &message)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Failed)
        ));
        let run = db::get_run(&pool, run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }
}
