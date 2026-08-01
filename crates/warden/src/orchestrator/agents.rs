use super::agent_run::SandboxGuard;
use super::diff::{read_diff, read_head_commit};
use super::gate_tail::protect_cycle_commit;
use super::tampering::agent_definition_tampering_finding;
use super::*;

/// Bounds how much of an agent's stderr is embedded in an error message — full output is already
/// logged via `tracing` before this is constructed.
const MAX_ERROR_STDERR_LEN: usize = 2000;

fn truncate_for_error(stderr: &str) -> String {
    if stderr.len() <= MAX_ERROR_STDERR_LEN {
        return stderr.to_string();
    }
    let boundary = stderr
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= MAX_ERROR_STDERR_LEN)
        .last()
        .unwrap_or(0);
    format!("{}… (truncated)", &stderr[..boundary])
}

fn step_succeeded(role: &Role, findings: &[Finding]) -> bool {
    !findings.iter().any(|finding| {
        finding.source == warden_core::FindingSource::role(role.as_str())
            && finding.severity == warden_core::Severity::Blocking
    })
}

/// The environment variable *names* forwarded to a `type: hook` workflow step's command.
const WORKFLOW_STEP_ENV_ALLOWLIST: &[&str] = &["HOME", "LANG", "TERM", "CARGO_HOME", "RUSTUP_HOME"];

fn blocking_step_finding(role: &Role, reason: String) -> Finding {
    Finding {
        source: warden_core::FindingSource::role(role.as_str()),
        severity: warden_core::Severity::Blocking,
        file: None,
        description: reason,
        action: Some(
            "fix the failing deterministic check; the pipeline retries once corrected".to_string(),
        ),
    }
}

impl Orchestrator {
    pub(super) async fn run_producer<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: ProducerInvocation<'_>,
    ) -> Result<ProducerCycleResult> {
        let ProducerInvocation {
            run_id,
            cycle_id,
            cycle_number,
            config,
            role,
            agent,
            env_allowlist,
            worktree_manager,
            base_commit,
            run_agent_definition_snapshot,
            prior_findings,
            cancel,
        } = invocation;

        let worktree = worktree_manager
            .create(run_id, role.as_str(), base_commit)
            .await?;
        db::set_cycle_worktree_path(
            &self.pool,
            cycle_id,
            role.as_str(),
            &worktree.path().display().to_string(),
        )
        .await?;

        let base_commit_sha = read_head_commit(worktree.path()).await?;

        let stdin_payload = warden_core::build_producer_input_json(
            role.as_str(),
            &agent.system_prompt,
            config.intent.clone(),
            prior_findings.to_vec(),
        )?;
        let outcome = self
            .run_agent(
                cycle_id,
                role,
                true,
                runner,
                &agent.command,
                env_allowlist,
                worktree.path(),
                &config.repo_path,
                &config.warden_home.join("worktrees").join(run_id),
                &agent.trusted_arg_values,
                stdin_payload,
                cancel,
            )
            .await?;

        if outcome.exit_code != 0 {
            if self.suspend_for_exhausted_quota(run_id).await? {
                return Err(WardenError::QuotaSuspended {
                    resets_at: db::get_run_rate_limit_status(&self.pool, run_id)
                        .await?
                        .expect("quota suspension requires a stored rate-limit status")
                        .resets_at,
                });
            }
            tracing::warn!(
                run_id,
                cycle_id,
                role = role.as_str(),
                exit_code = outcome.exit_code,
                stderr = %outcome.stderr,
                "producer step exited with a non-zero status; failing the run"
            );
            // Write-ahead: persist Failed before returning the error to the caller.
            self.transition(run_id, RunState::Failed).await?;
            self.publish_event(RunEvent::RunFinished {
                final_state: RunState::Failed.as_str().to_string(),
            })
            .await?;
            if let Err(error) = worktree.remove().await {
                tracing::warn!(%error, "failed to clean up producer worktree after a failed run");
            }
            return Err(WardenError::CoderFailed {
                run_id: run_id.to_string(),
                cycle_id: cycle_id.to_string(),
                exit_code: outcome.exit_code,
                stderr: truncate_for_error(&outcome.stderr),
            });
        }

        let new_commit = read_head_commit(worktree.path()).await?;

        let diff = read_diff(worktree.path(), &base_commit_sha, &new_commit).await?;

        let definition_tampering_finding = agent_definition_tampering_finding(
            worktree_manager,
            run_id,
            &new_commit,
            run_agent_definition_snapshot,
        )
        .await?;

        protect_cycle_commit(&config.repo_path, run_id, cycle_number, &new_commit).await?;
        db::set_cycle_commit_sha(&self.pool, cycle_id, &new_commit).await?;

        if let Err(error) = worktree.remove().await {
            tracing::warn!(%error, "failed to clean up producer worktree after cycle");
        }

        Ok(ProducerCycleResult {
            commit: new_commit,
            diff,
            definition_tampering_finding,
        })
    }

    pub(super) async fn run_gated_step<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: GatedStepInvocation<'_>,
    ) -> Result<Vec<Finding>> {
        match invocation.kind {
            warden_core::StepKind::Agent => self.run_gated_agent_step(runner, invocation).await,
            warden_core::StepKind::Hook => self.run_gated_hook_step(invocation).await,
        }
    }

    /// The `type: agent` half of [`Orchestrator::run_gated_step`]'s dispatch.
    async fn run_gated_agent_step<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: GatedStepInvocation<'_>,
    ) -> Result<Vec<Finding>> {
        let GatedStepInvocation {
            run_id,
            cycle_id,
            cycle_number,
            step_index,
            role,
            kind: _,
            agent,
            run: _,
            env_allowlist,
            worktree_manager,
            commit,
            diff,
            prior_findings,
            scope,
            captures_evidence,
            config,
            cancel,
        } = invocation;
        let agent = agent.expect(
            "run_gated_agent_step is only reached for StepKind::Agent, which always carries a \
             resolved agent -- ResolvedAgents::resolve's own invariant",
        );

        let gate = config.workflow.steps[step_index as usize].gate;
        let scoping_is_legal_for_this_step =
            step_index == 1 || gate == warden_core::Gate::ScopedReReview;
        if scope == warden_core::ReviewScope::Correctif && !scoping_is_legal_for_this_step {
            return Err(WardenError::Core(
                warden_core::CoreError::MalformedAgentInput(format!(
                    "step {step_index} ({role}) cannot be invoked with a scoped (\"correctif\") \
                     review -- only the first gated step (index 1), or a step whose own gate is \
                     \"scoped-re-review\", can be scoped"
                )),
            ));
        }

        let worktree = worktree_manager
            .create(run_id, role.as_str(), commit)
            .await?;
        db::set_cycle_worktree_path(
            &self.pool,
            cycle_id,
            role.as_str(),
            &worktree.path().display().to_string(),
        )
        .await?;

        let stdin_payload = warden_core::build_finding_agent_input_json(
            role.as_str(),
            &agent.system_prompt,
            commit,
            diff,
            prior_findings.to_vec(),
            scope,
        )?;

        let outcome = self
            .run_agent(
                cycle_id,
                role,
                false,
                runner,
                &agent.command,
                env_allowlist,
                worktree.path(),
                &config.repo_path,
                &config.warden_home.join("worktrees").join(run_id),
                &agent.trusted_arg_values,
                stdin_payload,
                cancel.clone(),
            )
            .await?;

        let findings = if outcome.exit_code != 0 {
            if self.suspend_for_exhausted_quota(run_id).await? {
                return Err(WardenError::QuotaSuspended {
                    resets_at: db::get_run_rate_limit_status(&self.pool, run_id)
                        .await?
                        .expect("quota suspension requires a stored rate-limit status")
                        .resets_at,
                });
            }
            tracing::warn!(
                run_id,
                cycle_id,
                role = role.as_str(),
                exit_code = outcome.exit_code,
                stderr = %outcome.stderr,
                "gated step exited non-zero; not trusting its stdout"
            );
            vec![Finding {
                source: warden_core::FindingSource::role(role.as_str()),
                severity: warden_core::Severity::Blocking,
                file: None,
                description: format!(
                    "{role} exited with status {} instead of 0 (stderr: {})",
                    outcome.exit_code,
                    truncate_for_error(&outcome.stderr)
                ),
                action: Some(
                    "investigate why the agent process exited non-zero and fix it".to_string(),
                ),
            }]
        } else {
            match runner
                .extract_findings(&outcome.stdout)
                .and_then(|findings| {
                    warden_core::validate_finding_sources_for_role(&findings, role)?;
                    Ok(findings)
                }) {
                Ok(findings) => findings,
                Err(parse_error) => {
                    tracing::warn!(%parse_error, role = role.as_str(), stdout = %outcome.stdout, "gated step produced unparsable or misattributed output");
                    vec![Finding {
                        source: warden_core::FindingSource::role(role.as_str()),
                        severity: warden_core::Severity::Blocking,
                        file: None,
                        description: format!(
                            "{role} produced unparsable or misattributed output: {parse_error}"
                        ),
                        action: Some("fix the agent's output format/finding sources".to_string()),
                    }]
                }
            }
        };

        // capture evidence right after a *successful* run of this step, still inside its worktree
        // -- which is about to be removed below, so this must happen before that, not after.
        if captures_evidence && step_succeeded(role, &findings) {
            self.capture_evidence_for_cycle(EvidenceCapture {
                run_id,
                cycle_id,
                cycle_number,
                config,
                tester_command: &agent.command,
                tester_worktree_path: worktree.path(),
                cancel,
            })
            .await;
        }

        if let Err(error) = worktree.remove().await {
            tracing::warn!(%error, role = role.as_str(), "failed to clean up worktree after cycle");
        }

        Ok(findings)
    }

    async fn run_gated_hook_step(
        &self,
        invocation: GatedStepInvocation<'_>,
    ) -> Result<Vec<Finding>> {
        let GatedStepInvocation {
            run_id,
            cycle_id,
            role,
            worktree_manager,
            commit,
            run,
            cancel,
            ..
        } = invocation;
        let command = run.expect(
            "run_gated_hook_step is only reached for StepKind::Hook, which always carries \
             \"run\" -- Workflow::parse_yaml's own invariant",
        );

        let worktree = worktree_manager
            .create(run_id, role.as_str(), commit)
            .await?;
        db::set_cycle_worktree_path(
            &self.pool,
            cycle_id,
            role.as_str(),
            &worktree.path().display().to_string(),
        )
        .await?;

        let findings = self
            .run_step_command(run_id, cycle_id, role, worktree.path(), command, cancel)
            .await?;

        if let Err(error) = worktree.remove().await {
            tracing::warn!(%error, role = role.as_str(), "failed to clean up worktree after cycle");
        }

        Ok(findings)
    }

    /// The sandboxed execution of one `type: hook` step's command, split out of
    /// [`Orchestrator::run_gated_hook_step`] purely for readability.
    async fn run_step_command(
        &self,
        run_id: &str,
        cycle_id: &str,
        role: &Role,
        cwd: &Path,
        command: &str,
        cancel: CancellationToken,
    ) -> Result<Vec<Finding>> {
        if let PolicyOutcome::Blocked { reason } =
            crate::hook::evaluate_shell_policy(&self.policy_gate, run_id, command).await
        {
            return Ok(vec![blocking_step_finding(role, reason)]);
        }

        let sandbox_id = self
            .sandbox
            .create_for_run(
                warden_sandbox::SandboxSpec {
                    cwd: cwd.to_path_buf(),
                },
                run_id,
            )
            .await?;

        let mut guard = SandboxGuard::new(Arc::clone(&self.sandbox), sandbox_id);

        let result: Result<Vec<Finding>> = async {
            let execution = self
                .sandbox
                .execute(
                    guard.id(),
                    warden_sandbox::Command {
                        program: "sh".to_string(),
                        args: vec!["-c".to_string(), command.to_string()],
                        env_allowlist: WORKFLOW_STEP_ENV_ALLOWLIST
                            .iter()
                            .map(|name| name.to_string())
                            .collect(),
                        stdin: None,
                    },
                    warden_sandbox::ExecuteOptions {
                        cancel,
                        on_stdout_line: None,
                    },
                )
                .await?;

            let pid = execution.pid.ok_or_else(|| ProcessError::MissingPid {
                command: command.to_string(),
            })?;

            let process_id = Uuid::new_v4().to_string();
            db::insert_agent_process(
                &self.pool,
                &process_id,
                cycle_id,
                role.as_str(),
                pid,
                &cwd.display().to_string(),
            )
            .await?;
            self.publish_event(RunEvent::AgentStarted {
                role: role.as_str().to_string(),
            })
            .await?;

            let waited = execution.wait().await;
            let exit_code_for_db = match &waited {
                Ok(output) => output.exit_code,
                Err(_) => -1,
            };
            db::mark_agent_process_ended(&self.pool, &process_id, exit_code_for_db).await?;
            let output = waited?;

            tracing::debug!(
                cycle_id,
                role = role.as_str(),
                command,
                exit_code = output.exit_code,
                stdout = %output.stdout,
                stderr = %output.stderr,
                "workflow step command output"
            );

            self.publish_event(RunEvent::AgentFinished {
                role: role.as_str().to_string(),
                exit_code: output.exit_code,
                usage: None,
            })
            .await?;

            if output.exit_code == 0 {
                return Ok(Vec::new());
            }

            let stderr_tail = crate::hook::trailing_chars(&output.stderr, 500);
            let stdout_tail = crate::hook::trailing_chars(&output.stdout, 500);
            let mut tail_parts = Vec::new();
            if !stderr_tail.is_empty() {
                tail_parts.push(format!("stderr: {stderr_tail}"));
            }
            if !stdout_tail.is_empty() {
                tail_parts.push(format!("stdout: {stdout_tail}"));
            }
            let reason = format!(
                "step command `{command}` exited {}{}",
                output.exit_code,
                crate::hook::format_tail_suffix(&tail_parts.join(" | "))
            );
            Ok(vec![blocking_step_finding(role, reason)])
        }
        .await;

        if let Err(error) = guard.destroy().await {
            tracing::warn!(
                cycle_id,
                role = role.as_str(),
                %error,
                "failed to destroy sandbox after workflow step command"
            );
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::test_support::*;
    use std::process::Command as SyncCommand;
    use tempfile::TempDir;

    fn reviewer_role() -> Role {
        Role::new("reviewer").unwrap()
    }

    fn tester_role() -> Role {
        Role::new("tester").unwrap()
    }

    #[tokio::test]
    async fn run_review_and_test_isolates_writes_to_different_worktree_files() {
        let repo = init_test_repo();
        std::fs::write(repo.path().join("review_target.txt"), "original-review\n").unwrap();
        std::fs::write(repo.path().join("test_target.txt"), "original-test\n").unwrap();
        let commit = |args: &[&str]| {
            let status = SyncCommand::new("git")
                .current_dir(repo.path())
                .args(args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        };
        commit(&["add", "."]);
        commit(&["commit", "--quiet", "-m", "add review/test targets"]);

        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();

        db::insert_run(
            &pool,
            "collision-run",
            &repo.path().display().to_string(),
            "main",
            "crossed findings, no collision",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "collision-cycle", "collision-run", 1)
            .await
            .unwrap();

        let reviewer_command = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"
                    echo modified-by-reviewer > review_target.txt
                    seen=$(cat test_target.txt)
                    echo "{\"source\":\"reviewer\",\"severity\":\"info\",\"description\":\"review_target=modified-by-reviewer test_target_seen=$seen\"}"
                    "#,
            ],
        );
        let tester_command = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"
                    echo modified-by-tester > test_target.txt
                    seen=$(cat review_target.txt)
                    echo "{\"source\":\"tester\",\"severity\":\"info\",\"description\":\"test_target=modified-by-tester review_target_seen=$seen\"}"
                    "#,
            ],
        );
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "crossed findings, no collision".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(reviewer_command),
                definition(tester_command),
            ],
            evidence_tool: None,
            evidence_store_in_repo: true,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();

        let orchestrator = Orchestrator::new(pool.clone());
        let reviewer_role = reviewer_role();
        let mut findings = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "collision-run",
                    cycle_id: "collision-cycle",
                    cycle_number: 1,
                    step_index: 1,
                    role: &reviewer_role,
                    agent: agents.steps[1].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: false,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        let tester_role = tester_role();
        findings.extend(
            orchestrator
                .run_gated_step(
                    &FakeCommandAdapter,
                    GatedStepInvocation {
                        run_id: "collision-run",
                        cycle_id: "collision-cycle",
                        cycle_number: 1,
                        step_index: 2,
                        role: &tester_role,
                        agent: agents.steps[2].as_ref(),
                        kind: warden_core::StepKind::Agent,
                        run: None,
                        env_allowlist: agents.env_allowlist,
                        worktree_manager: &worktree_manager,
                        commit: "HEAD",
                        diff: "",
                        prior_findings: &[],
                        scope: warden_core::ReviewScope::Full,
                        captures_evidence: true,
                        config: &config,
                        cancel: CancellationToken::new(),
                    },
                )
                .await
                .unwrap(),
        );

        assert_eq!(findings.len(), 2);
        let reviewer_finding = findings
            .iter()
            .find(|f| f.source == warden_core::FindingSource::role("reviewer"))
            .expect("reviewer finding present");
        let tester_finding = findings
            .iter()
            .find(|f| f.source == warden_core::FindingSource::role("tester"))
            .expect("tester finding present");

        assert!(
            reviewer_finding
                .description
                .contains("test_target_seen=original-test"),
            "reviewer's worktree must still see the untouched original \
                 test_target.txt, not the tester's write -- got: {}",
            reviewer_finding.description
        );
        assert!(
            tester_finding
                .description
                .contains("review_target_seen=original-review"),
            "tester's worktree must still see the untouched original \
                 review_target.txt, not the reviewer's write -- got: {}",
            tester_finding.description
        );
    }

    #[tokio::test]
    async fn a_step_1_invocation_with_a_correctif_scope_sends_a_scoped_payload() {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;
        let payloads = TempDir::new().unwrap();

        db::insert_run(
            &pool,
            "scoped-run",
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
        db::insert_cycle(&pool, "scoped-cycle", "scoped-run", 1)
            .await
            .unwrap();

        let capturing_reviewer = AgentCommand::new(
            "sh",
            [
                "-c",
                &format!("cat > '{}/reviewer.json'", payloads.path().display()),
            ],
        );
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(capturing_reviewer),
                definition(always_passing_tester()),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let originating_finding = Finding {
            source: warden_core::FindingSource::role("reviewer"),
            severity: warden_core::Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "unchecked unwrap".to_string(),
            action: Some("handle the error".to_string()),
        };

        let reviewer_role = reviewer_role();
        orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "scoped-run",
                    cycle_id: "scoped-cycle",
                    cycle_number: 1,
                    step_index: 1,
                    role: &reviewer_role,
                    agent: agents.steps[1].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "diff --git a/x b/x\n+fixed the unwrap\n",
                    prior_findings: std::slice::from_ref(&originating_finding),
                    scope: warden_core::ReviewScope::Correctif,
                    captures_evidence: false,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        let raw = std::fs::read_to_string(payloads.path().join("reviewer.json"))
            .expect("reviewer payload must have been captured");
        let payload = warden_core::parse_agent_input_message(&raw)
            .expect("a payload warden's own parser accepts");

        assert_eq!(payload.scope, warden_core::ReviewScope::Correctif);
        assert_eq!(
            payload.diff.as_deref(),
            Some("diff --git a/x b/x\n+fixed the unwrap\n")
        );
        assert_eq!(payload.findings, vec![originating_finding]);
    }

    #[tokio::test]
    async fn run_gated_step_rejects_a_correctif_scope_for_a_non_first_gated_step() {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;

        db::insert_run(
            &pool,
            "bad-scope-run",
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
        db::insert_cycle(&pool, "bad-scope-cycle", "bad-scope-run", 1)
            .await
            .unwrap();

        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(always_passing_tester()),
                definition(always_passing_tester()),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let tester_role = tester_role();
        let result = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "bad-scope-run",
                    cycle_id: "bad-scope-cycle",
                    cycle_number: 1,
                    step_index: 2,
                    role: &tester_role,
                    agent: agents.steps[2].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Correctif,
                    captures_evidence: true,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await;

        assert!(
            matches!(
                result,
                Err(WardenError::Core(
                    warden_core::CoreError::MalformedAgentInput(_)
                ))
            ),
            "expected a typed rejection, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn run_review_and_test_runs_reviewer_and_tester_sequentially_not_concurrently() {
        const SLEEP: Duration = Duration::from_millis(500);

        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();

        let sleepy_agent = AgentCommand::new("sh", ["-c", "sleep 0.5"]);
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "timing check".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(sleepy_agent.clone()),
                definition(sleepy_agent),
            ],
            evidence_tool: None,
            evidence_store_in_repo: true,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };

        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();

        let orchestrator = Orchestrator::new(pool.clone());

        db::insert_run(
            &pool,
            "timing-run",
            &repo.path().display().to_string(),
            "main",
            "timing check",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "timing-cycle", "timing-run", 1)
            .await
            .unwrap();

        let reviewer_role = reviewer_role();
        let tester_role = tester_role();
        let start = std::time::Instant::now();
        orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "timing-run",
                    cycle_id: "timing-cycle",
                    cycle_number: 1,
                    step_index: 1,
                    role: &reviewer_role,
                    agent: agents.steps[1].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: false,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "timing-run",
                    cycle_id: "timing-cycle",
                    cycle_number: 1,
                    step_index: 2,
                    role: &tester_role,
                    agent: agents.steps[2].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: true,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed > SLEEP.mul_f64(1.5),
            "expected the two run_gated_step calls ({elapsed:?}) to together take \
                 meaningfully longer than a single {SLEEP:?} sleep -- this looks \
                 like reviewer/tester ran concurrently instead of sequentially"
        );
    }

    async fn finding_agent_test_fixture() -> (TempDir, TempDir, TempDir, SqlitePool, WorktreeManager)
    {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();
        (repo, warden_home, db_dir, pool, worktree_manager)
    }

    #[tokio::test]
    async fn a_reviewer_forging_the_warden_finding_source_is_rejected_not_accepted() {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;

        db::insert_run(
            &pool,
            "forge-run",
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
        db::insert_cycle(&pool, "forge-cycle", "forge-run", 1)
            .await
            .unwrap();

        let forging_reviewer = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"echo '{"source":"warden","severity":"blocking","description":"fake tampering claim"}'"#,
            ],
        );
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(forging_reviewer),
                definition(always_passing_tester()),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let reviewer_role = reviewer_role();
        let findings = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "forge-run",
                    cycle_id: "forge-cycle",
                    cycle_number: 1,
                    step_index: 1,
                    role: &reviewer_role,
                    agent: agents.steps[1].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: false,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].source,
            warden_core::FindingSource::role("reviewer"),
            "a forged source must never reach the returned findings unchanged: {findings:?}"
        );
        assert_eq!(findings[0].severity, warden_core::Severity::Blocking);
        assert!(
            findings[0].description.contains("warden"),
            "the replacement finding should name what was forged, for diagnosability: {}",
            findings[0].description
        );
    }

    #[tokio::test]
    async fn a_tester_mislabelling_its_own_failure_as_the_reviewer_source_still_blocks_tester_succeeded(
    ) {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;

        db::insert_run(
            &pool,
            "mislabel-run",
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
        db::insert_cycle(&pool, "mislabel-cycle", "mislabel-run", 1)
            .await
            .unwrap();

        let self_mislabelling_tester = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"echo '{"source":"reviewer","severity":"blocking","description":"secretly failing"}'"#,
            ],
        );
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(always_passing_tester()),
                definition(self_mislabelling_tester),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let tester_role = tester_role();
        let findings = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "mislabel-run",
                    cycle_id: "mislabel-cycle",
                    cycle_number: 1,
                    step_index: 2,
                    role: &tester_role,
                    agent: agents.steps[2].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: true,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].source,
            warden_core::FindingSource::role("tester"),
            "the tester's own mislabelled finding must be re-attributed to Tester, not left as \
                 the forged Reviewer source: {findings:?}"
        );
        assert_eq!(findings[0].severity, warden_core::Severity::Blocking);
        assert!(
            !step_succeeded(&tester_role, &findings),
            "Minor 2: a tester that mislabels its own failure must still be seen as failed by \
                 step_succeeded, the gate that decides whether to trigger evidence capture"
        );
    }

    #[tokio::test]
    async fn a_tester_that_exits_nonzero_with_no_output_synthesizes_a_blocking_finding_not_a_silent_pass(
    ) {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;

        db::insert_run(
            &pool,
            "tester-crash-run",
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
        db::insert_cycle(&pool, "tester-crash-cycle", "tester-crash-run", 1)
            .await
            .unwrap();

        let crashing_tester = AgentCommand::new("sh", ["-c", "printf 'boom' >&2; exit 7"]);
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(always_passing_tester()),
                definition(crashing_tester),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let tester_role = tester_role();
        let findings = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "tester-crash-run",
                    cycle_id: "tester-crash-cycle",
                    cycle_number: 1,
                    step_index: 2,
                    role: &tester_role,
                    agent: agents.steps[2].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: true,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            findings.len(),
            1,
            "a crashing tester must synthesize exactly one blocking finding, not be silently \
                 read as zero findings: {findings:?}"
        );
        assert_eq!(
            findings[0].source,
            warden_core::FindingSource::role("tester")
        );
        assert_eq!(findings[0].severity, warden_core::Severity::Blocking);
        assert!(
            findings[0].description.contains("exited with status 7"),
            "the synthesized finding should name the actual exit status: {}",
            findings[0].description
        );
        assert!(
            !step_succeeded(&tester_role, &findings),
            "a tester that crashed non-zero must never be read as a passing test suite by \
                 step_succeeded, the gate that decides whether to trigger evidence capture"
        );
    }

    #[tokio::test]
    async fn a_reviewer_finding_with_its_own_correct_source_passes_through_unchanged() {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;

        db::insert_run(
            &pool,
            "legit-run",
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
        db::insert_cycle(&pool, "legit-cycle", "legit-run", 1)
            .await
            .unwrap();

        let honest_reviewer = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"echo '{"source":"reviewer","severity":"warning","description":"looks mostly fine","file":"src/lib.rs"}'"#,
            ],
        );
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(honest_reviewer),
                definition(always_passing_tester()),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let reviewer_role = reviewer_role();
        let findings = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "legit-run",
                    cycle_id: "legit-cycle",
                    cycle_number: 1,
                    step_index: 1,
                    role: &reviewer_role,
                    agent: agents.steps[1].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: false,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            findings,
            vec![Finding {
                source: warden_core::FindingSource::role("reviewer"),
                severity: warden_core::Severity::Warning,
                file: Some("src/lib.rs".to_string()),
                description: "looks mostly fine".to_string(),
                action: None,
            }]
        );
    }

    async fn orchestrator_with_run_and_cycle(
        pool: &SqlitePool,
        run_id: &str,
        cycle_id: &str,
    ) -> Orchestrator {
        db::insert_run(pool, run_id, "/tmp/repo", "main", "intent", 3, 3, 3, 5)
            .await
            .unwrap();
        db::insert_cycle(pool, cycle_id, run_id, 1).await.unwrap();
        Orchestrator::new(pool.clone())
    }

    #[tokio::test]
    async fn a_hook_steps_command_is_cancelled_when_the_run_cancellation_token_fires() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let orchestrator =
            orchestrator_with_run_and_cycle(&pool, "cancel-hook-run", "cancel-hook-cycle").await;
        let cwd = TempDir::new().unwrap();
        let role = Role::new("lint").unwrap();

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        let result = orchestrator
            .run_step_command(
                "cancel-hook-run",
                "cancel-hook-cycle",
                &role,
                cwd.path(),
                "sleep 30",
                cancel,
            )
            .await;

        assert!(
            matches!(
                result,
                Err(WardenError::Sandbox(
                    warden_sandbox::SandboxError::Cancelled { .. }
                ))
            ),
            "expected the hook step's command to be cancelled, got {result:?}"
        );
    }

    #[tokio::test]
    async fn a_hook_steps_command_only_forwards_the_narrow_workflow_step_allowlist() {
        assert!(
            std::env::var("CARGO_MANIFEST_DIR").is_ok(),
            "precondition: cargo test sets CARGO_MANIFEST_DIR"
        );

        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let orchestrator =
            orchestrator_with_run_and_cycle(&pool, "env-hook-run", "env-hook-cycle").await;
        let cwd = TempDir::new().unwrap();
        let role = Role::new("lint").unwrap();

        let findings = orchestrator
            .run_step_command(
                "env-hook-run",
                "env-hook-cycle",
                &role,
                cwd.path(),
                r#"if [ -n "$CARGO_MANIFEST_DIR" ]; then echo leaked; exit 1; fi"#,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(
            findings.is_empty(),
            "CARGO_MANIFEST_DIR is not on WORKFLOW_STEP_ENV_ALLOWLIST and must never reach the \
                 command's own environment: {findings:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_command_with_both_stdout_and_stderr_includes_both_tails() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let orchestrator =
            orchestrator_with_run_and_cycle(&pool, "both-tails-run", "both-tails-cycle").await;
        let cwd = TempDir::new().unwrap();
        let role = Role::new("lint").unwrap();

        let findings = orchestrator
            .run_step_command(
                "both-tails-run",
                "both-tails-cycle",
                &role,
                cwd.path(),
                "echo out-marker; echo err-marker >&2; exit 1",
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].description.contains("out-marker")
                && findings[0].description.contains("err-marker"),
            "expected both the stdout and stderr tails in the description: {}",
            findings[0].description
        );
    }

    #[test]
    fn workflow_step_env_allowlist_is_pinned() {
        assert_eq!(
            WORKFLOW_STEP_ENV_ALLOWLIST,
            &["HOME", "LANG", "TERM", "CARGO_HOME", "RUSTUP_HOME"]
        );
    }

    #[tokio::test]
    async fn a_hook_steps_sandbox_is_destroyed_after_its_command_completes() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        db::insert_run(
            &pool,
            "destroy-hook-run",
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
        db::insert_cycle(&pool, "destroy-hook-cycle", "destroy-hook-run", 1)
            .await
            .unwrap();
        let sandbox = Arc::new(RecordingSandbox::new(false));
        let orchestrator =
            Orchestrator::new(pool.clone()).with_sandbox(sandbox.clone() as Arc<dyn Sandbox>);
        let cwd = TempDir::new().unwrap();
        let role = Role::new("lint").unwrap();

        orchestrator
            .run_step_command(
                "destroy-hook-run",
                "destroy-hook-cycle",
                &role,
                cwd.path(),
                "true",
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            sandbox.calls(),
            vec!["create", "execute", "destroy"],
            "the sandbox created for the hook step's command must be destroyed exactly once"
        );
    }

    #[tokio::test]
    async fn a_hook_steps_sandbox_is_destroyed_when_run_step_command_is_dropped_mid_flight() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        db::insert_run(
            &pool,
            "abort-hook-run",
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
        db::insert_cycle(&pool, "abort-hook-cycle", "abort-hook-run", 1)
            .await
            .unwrap();
        let sandbox = Arc::new(RecordingSandbox::new(false));
        let orchestrator = Arc::new(
            Orchestrator::new(pool.clone()).with_sandbox(sandbox.clone() as Arc<dyn Sandbox>),
        );
        let orchestrator_for_task = Arc::clone(&orchestrator);
        let cwd = TempDir::new().unwrap();
        let cwd_path = cwd.path().to_path_buf();

        let handle = tokio::spawn(async move {
            let role = Role::new("lint").unwrap();
            let _ = orchestrator_for_task
                .run_step_command(
                    "abort-hook-run",
                    "abort-hook-cycle",
                    &role,
                    &cwd_path,
                    "sleep 30",
                    CancellationToken::new(),
                )
                .await;
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();
        let _ = handle.await;

        for _ in 0..200 {
            if sandbox.calls().contains(&"destroy") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let calls = sandbox.calls();
        assert!(
            calls.contains(&"create"),
            "expected the sandbox to have been created before the abort, got {calls:?}"
        );
        assert!(
            calls.contains(&"destroy"),
            "expected `SandboxGuard::drop`'s backstop to destroy the sandbox created for a \
                 `type: hook` step's command whose future was dropped mid-flight, got {calls:?}"
        );
    }
}
