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

        self.transition(run_id, RunState::Pushed).await?;
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
                db::set_run_current_review_cycle(&self.pool, run_id, charged).await?;
                decide_next_state_after_ci(CiOutcome::ChecksFailed, charged, run.max_review_cycles)
            }
            Some(ci_outcome) => decide_next_state_after_ci(
                ci_outcome,
                run.current_review_cycle,
                run.max_review_cycles,
            ),
            None => RunState::Failed,
        };
        self.transition(run_id, next_state).await?;

        match next_state {
            RunState::CoderRunning => Ok(PostConvergenceOutcome::Reboucle {
                findings: message.outcome.findings()?,
            }),
            terminal => Ok(PostConvergenceOutcome::Terminal(terminal)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::test_support::*;
    use crate::policy_gate::{ApprovalGate, ApprovalRequest};
    use std::process::Command as SyncCommand;
    use tempfile::TempDir;

    fn init_bare_repo_fixture() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        let status = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["init", "--bare", "--quiet"])
            .status()
            .expect("spawn git");
        assert!(status.success());
        dir
    }

    struct FakeGateTrigger {
        outcome: warden_core::CiWatchOutcome,
        pr_number: Option<u64>,
    }

    impl GateTrigger for FakeGateTrigger {
        async fn trigger_run_tail(&self, request: &RunTailTrigger<'_>) -> Result<GateChild> {
            self.deliver(request.run_id, request.ci_result_socket)
                .await?;
            Ok(GateChild::never_exiting())
        }

        async fn trigger_resume_watch(
            &self,
            run_id: &str,
            _pr_number: u64,
            ci_result_socket: &Path,
        ) -> Result<GateChild> {
            self.deliver(run_id, ci_result_socket).await?;
            Ok(GateChild::never_exiting())
        }
    }

    impl FakeGateTrigger {
        async fn deliver(&self, run_id: &str, ci_result_socket: &Path) -> Result<()> {
            use tokio::io::AsyncWriteExt;

            let message = CiResultMessage {
                run_id: run_id.to_string(),
                pr_number: self.pr_number,
                outcome: self.outcome.clone(),
            };
            let json = message.to_json()?;
            let mut stream = tokio::net::UnixStream::connect(ci_result_socket).await?;
            stream.write_all(json.as_bytes()).await?;
            stream.shutdown().await?;
            Ok(())
        }
    }

    async fn converged_run_fixture(
        pool: &SqlitePool,
        repo: &TempDir,
        bare_repo: &TempDir,
    ) -> (String, RunConfig, String) {
        let run_id = Uuid::new_v4().to_string();
        db::insert_run(pool, &run_id, "/tmp/repo", "main", "intent", 5, 5, 3, 5)
            .await
            .unwrap();
        db::update_run_state(pool, &run_id, RunState::CoderRunning)
            .await
            .unwrap();
        db::update_run_state(pool, &run_id, RunState::RunningStep(1))
            .await
            .unwrap();
        db::update_run_state(pool, &run_id, RunState::RunningStep(2))
            .await
            .unwrap();
        db::update_run_state(pool, &run_id, RunState::Converged)
            .await
            .unwrap();

        let head_output = SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let converged_commit = String::from_utf8_lossy(&head_output.stdout)
            .trim()
            .to_string();
        db::set_run_converged_commit(pool, &run_id, &converged_commit)
            .await
            .unwrap();

        let warden_home = TempDir::new().unwrap();
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 5,
            max_test_cycles: 5,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(AgentCommand::new("sh", ["-c", "true"])),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: Some(GateConfig {
                bare_repo_path: bare_repo.path().to_path_buf(),
                gated_bin: PathBuf::from("/unused/in/this/test"),
                repo_slug: None,
                poll_interval_secs: 1,
                inactivity_timeout_secs: 3600,
            }),
            untrusted_repo_agent_definitions: Vec::new(),
        };
        std::mem::forget(warden_home);

        (run_id, config, converged_commit)
    }

    struct UnreachableGateTrigger;

    impl GateTrigger for UnreachableGateTrigger {
        async fn trigger_run_tail(&self, _request: &RunTailTrigger<'_>) -> Result<GateChild> {
            panic!("trigger_run_tail must never run once the policy has denied the push");
        }

        async fn trigger_resume_watch(
            &self,
            _run_id: &str,
            _pr_number: u64,
            _ci_result_socket: &Path,
        ) -> Result<GateChild> {
            panic!("trigger_resume_watch must never run in this test");
        }
    }

    fn assert_no_staging_ref(bare_repo: &TempDir, run_id: &str) {
        let check = SyncCommand::new("git")
            .current_dir(bare_repo.path())
            .args([
                "rev-parse",
                "--verify",
                &format!("refs/warden-staging/{run_id}"),
            ])
            .output()
            .unwrap();
        assert!(
            !check.status.success(),
            "the converged commit must never be staged once the policy has blocked the push"
        );
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_fails_the_run_when_policy_denies_the_push() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let rules =
            warden_policy::RuleSet::from_yaml("rules:\n  - action: git_push\n    deny: [main]\n")
                .unwrap();
        let orchestrator = Orchestrator::new(pool.clone()).with_policy_gate(Arc::new(
            PolicyGate::new(warden_policy::Evaluator::new(rules)),
        ));

        let outcome = orchestrator
            .drive_post_convergence_tail(
                &run_id,
                &config,
                &converged_commit,
                &UnreachableGateTrigger,
            )
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Failed)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
        assert_no_staging_ref(&bare_repo, &run_id);
    }

    struct FakeApprovalGate {
        approve: bool,
    }

    #[async_trait::async_trait]
    impl ApprovalGate for FakeApprovalGate {
        async fn approve(&self, _request: ApprovalRequest<'_>) -> bool {
            self.approve
        }
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_pushes_once_a_required_approval_is_granted() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let rules = warden_policy::RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    require: [tests]\n",
        )
        .unwrap();
        let policy_gate = PolicyGate::new(warden_policy::Evaluator::new(rules))
            .with_approval_gate(Arc::new(FakeApprovalGate { approve: true }));
        let orchestrator = Orchestrator::new(pool.clone()).with_policy_gate(Arc::new(policy_gate));
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_passed(),
            pr_number: Some(42),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Done)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Done);
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_fails_the_run_when_a_required_approval_is_refused() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let rules = warden_policy::RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    require: [tests]\n",
        )
        .unwrap();
        let policy_gate = PolicyGate::new(warden_policy::Evaluator::new(rules))
            .with_approval_gate(Arc::new(FakeApprovalGate { approve: false }));
        let orchestrator = Orchestrator::new(pool.clone()).with_policy_gate(Arc::new(policy_gate));

        let outcome = orchestrator
            .drive_post_convergence_tail(
                &run_id,
                &config,
                &converged_commit,
                &UnreachableGateTrigger,
            )
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Failed)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
        assert_no_staging_ref(&bare_repo, &run_id);
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_reaches_done_on_checks_passed() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_passed(),
            pr_number: Some(42),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Done)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Done);
        assert_eq!(run.pr_number, Some(42));
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_stages_the_commit_outside_the_notify_hooks_ref_namespace()
    {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let ci_finding = Finding {
            source: warden_core::FindingSource::Ci,
            severity: warden_core::Severity::Blocking,
            file: None,
            description: "build failed".to_string(),
            action: None,
        };
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_failed(&[ci_finding]),
            pr_number: Some(42),
        };
        orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        let staging_ref_check = SyncCommand::new("git")
            .current_dir(bare_repo.path())
            .args([
                "rev-parse",
                "--verify",
                &format!("refs/warden-staging/{run_id}"),
            ])
            .output()
            .unwrap();
        assert!(
            staging_ref_check.status.success(),
            "the converged commit must be staged under refs/warden-staging/<run_id>"
        );

        let notify_ref_check = SyncCommand::new("git")
            .current_dir(bare_repo.path())
            .args([
                "rev-parse",
                "--verify",
                &format!("refs/heads/warden-run/{run_id}"),
            ])
            .output()
            .unwrap();
        assert!(
            !notify_ref_check.status.success(),
            "the converged commit must NOT be staged under refs/heads/warden-run/<run_id> -- \
                 that ref is what the notify hook/serve daemon watch for a push-notification, and \
                 would auto-push this content straight to origin on a deployed gate"
        );
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_reclaims_the_staging_ref_on_a_terminal_outcome() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_passed(),
            pr_number: Some(42),
        };
        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Done)
        ));

        let staging_ref_check = SyncCommand::new("git")
            .current_dir(bare_repo.path())
            .args([
                "rev-parse",
                "--verify",
                &format!("refs/warden-staging/{run_id}"),
            ])
            .output()
            .unwrap();
        assert!(
            !staging_ref_check.status.success(),
            "the staging ref must be reclaimed once the run reaches a terminal state"
        );
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_reboucles_to_coder_running_with_ci_findings_on_checks_failed(
    ) {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let ci_finding = Finding {
            source: warden_core::FindingSource::Ci,
            severity: warden_core::Severity::Blocking,
            file: None,
            description: "build failed".to_string(),
            action: None,
        };
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_failed(&[ci_finding]),
            pr_number: Some(7),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        match outcome {
            PostConvergenceOutcome::Reboucle { findings } => {
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].source, warden_core::FindingSource::Ci);
                assert_eq!(findings[0].description, "build failed");
            }
            other => panic!("expected Reboucle, got {other:?}"),
        }
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::CoderRunning);
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_maps_gate_failed_to_failed() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::gate_failed("skeleton push failed"),
            pr_number: None,
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Failed)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_fails_the_run_when_warden_gated_dies_without_delivering() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = NeverDeliversGateTrigger;

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(
            matches!(outcome, PostConvergenceOutcome::Terminal(RunState::Failed)),
            "a gated child that exits without delivering must fail the run, not hang it"
        );
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    struct NeverDeliversGateTrigger;

    impl GateTrigger for NeverDeliversGateTrigger {
        async fn trigger_run_tail(&self, _request: &RunTailTrigger<'_>) -> Result<GateChild> {
            Ok(GateChild::already_exited())
        }

        async fn trigger_resume_watch(
            &self,
            _run_id: &str,
            _pr_number: u64,
            _ci_result_socket: &Path,
        ) -> Result<GateChild> {
            Ok(GateChild::already_exited())
        }
    }

    #[tokio::test]
    async fn resume_awaiting_ci_runs_fails_the_run_when_the_ci_result_never_arrives() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let warden_home = TempDir::new().unwrap();

        db::insert_run(
            &pool,
            "run-silent",
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
        for state in [
            RunState::CoderRunning,
            RunState::RunningStep(1),
            RunState::RunningStep(2),
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
        ] {
            db::update_run_state(&pool, "run-silent", state)
                .await
                .unwrap();
        }
        db::set_run_pr_number(&pool, "run-silent", 42)
            .await
            .unwrap();

        let trigger = NeverDeliversGateTrigger;

        let resumed = resume_awaiting_ci_runs(
            pool.clone(),
            warden_home.path().to_path_buf(),
            trigger,
            warden_home.path().to_path_buf(),
        )
        .await
        .unwrap();

        assert_eq!(resumed, vec!["run-silent".to_string()]);
        let run = db::get_run(&pool, "run-silent").await.unwrap().unwrap();
        assert_eq!(
            run.state,
            RunState::Failed,
            "a run stuck in AwaitingCi with no terminal message ever delivered must be failed \
                 outright once the bounded wait expires, not left hanging"
        );
    }

    #[tokio::test]
    async fn apply_ci_result_message_is_a_noop_once_the_run_already_left_awaiting_ci() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let run_id = Uuid::new_v4().to_string();
        db::insert_run(&pool, &run_id, "/tmp/repo", "main", "intent", 5, 5, 3, 5)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::CoderRunning)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::RunningStep(1))
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::RunningStep(2))
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::Converged)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::Pushed)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::AwaitingCi)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::Done)
            .await
            .unwrap();

        let orchestrator = Orchestrator::new(pool.clone());
        let message = CiResultMessage {
            run_id: run_id.clone(),
            pr_number: Some(99),
            outcome: warden_core::CiWatchOutcome::checks_passed(),
        };

        let outcome = orchestrator
            .apply_ci_result_message(&run_id, &message)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Done)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Done);
        assert_eq!(
            run.pr_number, None,
            "a stale delivery must not even record its pr_number once ignored"
        );
    }

    #[tokio::test]
    async fn apply_ci_result_message_rejects_a_run_id_mismatch() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        db::insert_run(&pool, "run-a", "/tmp/repo", "main", "intent", 5, 5, 3, 5)
            .await
            .unwrap();
        for state in [
            RunState::CoderRunning,
            RunState::RunningStep(1),
            RunState::RunningStep(2),
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
        ] {
            db::update_run_state(&pool, "run-a", state).await.unwrap();
        }

        let orchestrator = Orchestrator::new(pool.clone());
        let message = CiResultMessage {
            run_id: "run-b".to_string(),
            pr_number: Some(99),
            outcome: warden_core::CiWatchOutcome::checks_passed(),
        };

        let result = orchestrator
            .apply_ci_result_message("run-a", &message)
            .await;

        assert!(matches!(
            result,
            Err(WardenError::CiResultRunIdMismatch { .. })
        ));
        let run = db::get_run(&pool, "run-a").await.unwrap().unwrap();
        assert_eq!(
            run.state,
            RunState::AwaitingCi,
            "a run_id mismatch must leave the run's state completely untouched"
        );
        assert_eq!(run.pr_number, None);
    }

    #[tokio::test]
    async fn recover_crashed_runs_leaves_awaiting_ci_runs_untouched() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(&pool, "run-ci", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::CoderRunning)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::RunningStep(1))
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::RunningStep(2))
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::Converged)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::Pushed)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::AwaitingCi)
            .await
            .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();

        assert!(
            failed.is_empty(),
            "AwaitingCi must never be marked Failed by recover_crashed_runs"
        );
        let run = db::get_run(&pool, "run-ci").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::AwaitingCi);
    }

    #[tokio::test]
    async fn resume_awaiting_ci_runs_resumes_the_watch_and_applies_its_outcome() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let warden_home = TempDir::new().unwrap();

        db::insert_run(&pool, "run-ci", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
            .await
            .unwrap();
        for state in [
            RunState::CoderRunning,
            RunState::RunningStep(1),
            RunState::RunningStep(2),
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
        ] {
            db::update_run_state(&pool, "run-ci", state).await.unwrap();
        }
        db::set_run_pr_number(&pool, "run-ci", 42).await.unwrap();

        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::merged(),
            pr_number: Some(42),
        };

        let resumed = resume_awaiting_ci_runs(
            pool.clone(),
            warden_home.path().to_path_buf(),
            trigger,
            warden_home.path().to_path_buf(),
        )
        .await
        .unwrap();

        assert_eq!(resumed, vec!["run-ci".to_string()]);
        let run = db::get_run(&pool, "run-ci").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Done);
    }

    #[tokio::test]
    async fn resume_awaiting_ci_runs_fails_a_run_with_no_recorded_pr_number() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let warden_home = TempDir::new().unwrap();

        db::insert_run(
            &pool,
            "run-no-pr",
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
        for state in [
            RunState::CoderRunning,
            RunState::RunningStep(1),
            RunState::RunningStep(2),
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
        ] {
            db::update_run_state(&pool, "run-no-pr", state)
                .await
                .unwrap();
        }

        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::merged(),
            pr_number: None,
        };

        let resumed = resume_awaiting_ci_runs(
            pool.clone(),
            warden_home.path().to_path_buf(),
            trigger,
            warden_home.path().to_path_buf(),
        )
        .await
        .unwrap();

        assert_eq!(resumed, vec!["run-no-pr".to_string()]);
        let run = db::get_run(&pool, "run-no-pr").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    struct RecordingGateTrigger {
        pool: SqlitePool,
        run_id: String,
        outcome: warden_core::CiWatchOutcome,
        pr_number: Option<u64>,
        observed_state_at_trigger: std::sync::Mutex<Option<RunState>>,
    }

    impl GateTrigger for RecordingGateTrigger {
        async fn trigger_run_tail(&self, request: &RunTailTrigger<'_>) -> Result<GateChild> {
            let run = db::get_run(&self.pool, &self.run_id)
                .await
                .unwrap()
                .unwrap();
            *self.observed_state_at_trigger.lock().unwrap() = Some(run.state);

            let pool = self.pool.clone();
            let run_id = self.run_id.clone();
            let socket_path = request.ci_result_socket.to_path_buf();
            let message = CiResultMessage {
                run_id: request.run_id.to_string(),
                pr_number: self.pr_number,
                outcome: self.outcome.clone(),
            };
            tokio::spawn(async move {
                for _ in 0..200 {
                    let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
                    if run.state == RunState::AwaitingCi {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
                assert_eq!(
                    run.state,
                    RunState::AwaitingCi,
                    "gave up waiting for AwaitingCi to be persisted before delivering"
                );

                use tokio::io::AsyncWriteExt;
                let json = message.to_json().unwrap();
                let mut stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
                stream.write_all(json.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            });
            Ok(GateChild::never_exiting())
        }

        async fn trigger_resume_watch(
            &self,
            _run_id: &str,
            _pr_number: u64,
            _ci_result_socket: &Path,
        ) -> Result<GateChild> {
            unreachable!("resume-watch is not exercised by this test")
        }
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_persists_pushed_then_awaiting_ci_before_the_terminal_message_is_ever_applied(
    ) {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = RecordingGateTrigger {
            pool: pool.clone(),
            run_id: run_id.clone(),
            outcome: warden_core::CiWatchOutcome::checks_passed(),
            pr_number: Some(11),
            observed_state_at_trigger: std::sync::Mutex::new(None),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert_eq!(
            *trigger.observed_state_at_trigger.lock().unwrap(),
            Some(RunState::Pushed),
            "Pushed must be durably persisted before the watch is even triggered"
        );
        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Done)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Done);
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_maps_checks_failed_at_cycle_budget_to_failed_not_max_cycles_exceeded(
    ) {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;
        db::set_run_current_review_cycle(&pool, &run_id, 4)
            .await
            .unwrap();

        let orchestrator = Orchestrator::new(pool.clone());
        let ci_finding = Finding {
            source: warden_core::FindingSource::Ci,
            severity: warden_core::Severity::Blocking,
            file: None,
            description: "flaky test at budget".to_string(),
            action: None,
        };
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_failed(&[ci_finding]),
            pr_number: Some(13),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(
            matches!(outcome, PostConvergenceOutcome::Terminal(RunState::Failed)),
            "expected Terminal(Failed), got {outcome:?}"
        );
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    #[tokio::test]
    async fn repeated_checks_failed_charges_the_review_budget_until_it_terminates_at_failed() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;
        let max = config.max_review_cycles;

        let orchestrator = Orchestrator::new(pool.clone());
        let ci_finding = Finding {
            source: warden_core::FindingSource::Ci,
            severity: warden_core::Severity::Blocking,
            file: None,
            description: "flaky CI".to_string(),
            action: None,
        };
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_failed(&[ci_finding]),
            pr_number: Some(99),
        };

        for expected_cycle in 1..max {
            let outcome = orchestrator
                .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
                .await
                .unwrap();
            assert!(
                matches!(outcome, PostConvergenceOutcome::Reboucle { .. }),
                "CI failure {expected_cycle} below budget must reboucle, got {outcome:?}"
            );
            let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
            assert_eq!(
                run.current_review_cycle, expected_cycle,
                "each CI reboucle must advance the review budget counter by exactly one"
            );
            assert_eq!(
                run.current_test_cycle, 0,
                "a CI reboucle is charged to the review budget, never the test budget"
            );
            db::update_run_state(&pool, &run_id, RunState::RunningStep(1))
                .await
                .unwrap();
            db::update_run_state(&pool, &run_id, RunState::RunningStep(2))
                .await
                .unwrap();
            db::update_run_state(&pool, &run_id, RunState::Converged)
                .await
                .unwrap();
        }

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();
        assert!(
            matches!(outcome, PostConvergenceOutcome::Terminal(RunState::Failed)),
            "CI failure at the review budget must terminate at Failed, got {outcome:?}"
        );
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
        assert_eq!(
            run.current_review_cycle, max,
            "the review budget is what ran out"
        );
        assert_eq!(
            run.current_test_cycle, 0,
            "CI reboucles never charge the test budget"
        );
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_maps_closed_to_failed() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::closed(),
            pr_number: Some(21),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Failed)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_maps_timed_out_to_failed() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::timed_out(),
            pr_number: Some(22),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Failed)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    #[cfg(unix)]
    fn plant_marker_hook(repo_path: &Path, hook_name: &str, marker: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let hooks_dir = repo_path.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join(hook_name);
        std::fs::write(
            &hook_path,
            format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn push_to_bare_repo_disables_a_planted_pre_push_hook() {
        let repo = init_test_repo();
        let bare_repo = TempDir::new().unwrap();
        let status = SyncCommand::new("git")
            .current_dir(bare_repo.path())
            .args(["init", "--bare", "--quiet"])
            .status()
            .unwrap();
        assert!(status.success());

        let marker = repo.path().join("pre-push-ran");
        plant_marker_hook(repo.path(), "pre-push", &marker);

        let head = SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let commit_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();

        push_converged_commit_to_bare_repo(
            repo.path(),
            bare_repo.path(),
            &commit_sha,
            "run-hook-1",
        )
        .await
        .unwrap();

        assert!(
            !marker.exists(),
            "a pre-push hook planted in repo_path must never run when warden itself pushes \
                 to the bare gate repo"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protect_cycle_commit_disables_a_planted_reference_transaction_hook() {
        let repo = init_test_repo();
        let marker = repo.path().join("reference-transaction-ran");
        plant_marker_hook(repo.path(), "reference-transaction", &marker);

        let head = SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let commit_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();

        protect_cycle_commit(repo.path(), "run-hook-2", 1, &commit_sha)
            .await
            .unwrap();

        assert!(
            !marker.exists(),
            "a reference-transaction hook planted in repo_path must never run when warden \
                 itself protects a cycle commit via update-ref"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worktree_create_disables_a_planted_post_checkout_hook() {
        let repo = init_test_repo();
        let worktrees_root = TempDir::new().unwrap();
        let marker = repo.path().join("post-checkout-ran");
        plant_marker_hook(repo.path(), "post-checkout", &marker);

        let manager = WorktreeManager::new(repo.path(), worktrees_root.path()).unwrap();
        let _worktree = manager.create("run-hook-3", "coder", "HEAD").await.unwrap();

        assert!(
            !marker.exists(),
            "a post-checkout hook planted in repo_path must never run when warden itself \
                 creates a role's worktree"
        );
    }
}
