use super::agent_run::SandboxGuard;
use super::diff::{read_diff, read_head_commit};
use super::gate_tail::protect_cycle_commit;
use super::tampering::agent_definition_tampering_finding;
use super::*;

const MAX_ERROR_STDERR_LEN: usize = 2000;
const WORKFLOW_STEP_ENV_ALLOWLIST: &[&str] = &["HOME", "LANG", "TERM", "CARGO_HOME", "RUSTUP_HOME"];

fn truncate_for_error(stderr: &str) -> String {
    if stderr.len() <= MAX_ERROR_STDERR_LEN {
        return stderr.to_string();
    }
    let boundary = stderr
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= MAX_ERROR_STDERR_LEN)
        .last()
        .unwrap_or(0);
    format!("{}… (truncated)", &stderr[..boundary])
}

fn blocking_finding(role: &Role, reason: String) -> Finding {
    Finding {
        source: warden_core::FindingSource::role(role.as_str()),
        severity: warden_core::Severity::Blocking,
        file: None,
        description: reason,
        action: None,
    }
}

impl Orchestrator {
    pub(super) async fn run_step<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: StepInvocation<'_>,
    ) -> Result<StepResult> {
        match invocation.kind {
            warden_core::StepKind::Agent => self.run_agent_step(runner, invocation).await,
            warden_core::StepKind::Command => self.run_command_step(invocation).await,
        }
    }

    async fn run_agent_step<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: StepInvocation<'_>,
    ) -> Result<StepResult> {
        let StepInvocation {
            run_id,
            cycle_id,
            cycle_number,
            step_index,
            config,
            role,
            agent,
            env_allowlist,
            worktree_manager,
            commit,
            run_base_commit,
            run_agent_definition_snapshot,
            prior_findings,
            cancel,
            ..
        } = invocation;
        let agent = agent.expect("agent steps carry a resolved definition");
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

        let input_diff = read_diff(worktree.path(), run_base_commit, commit).await?;
        let stdin_payload = warden_core::build_step_input_json(
            role.as_str(),
            &agent.system_prompt,
            &config.intent,
            commit,
            input_diff,
            prior_findings.to_vec(),
        )?;
        let outcome = self
            .run_agent(
                cycle_id,
                role,
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

        let new_commit = read_head_commit(worktree.path()).await?;
        let mut findings = Vec::new();
        let mut step_outcome = warden_core::StepOutcome::Clean;
        if outcome.exit_code != 0 {
            if self.suspend_for_exhausted_quota(run_id).await? {
                let resets_at = db::get_run_rate_limit_status(&self.pool, run_id)
                    .await?
                    .expect("quota suspension requires stored status")
                    .resets_at;
                worktree.remove().await?;
                return Err(WardenError::QuotaSuspended { resets_at });
            }
            tracing::warn!(
                run_id,
                cycle_id,
                role = role.as_str(),
                exit_code = outcome.exit_code,
                stderr = %outcome.stderr,
                "agent step failed"
            );
            step_outcome = warden_core::StepOutcome::Error;
            findings.push(blocking_finding(
                role,
                format!(
                    "agent exited with status {}: {}",
                    outcome.exit_code,
                    truncate_for_error(&outcome.stderr)
                ),
            ));
        } else {
            match runner.extract_findings(&outcome.stdout).and_then(|items| {
                warden_core::validate_finding_sources_for_role(&items, role)?;
                Ok(items)
            }) {
                Ok(items) => {
                    if items
                        .iter()
                        .any(|finding| finding.severity == warden_core::Severity::Blocking)
                    {
                        step_outcome = warden_core::StepOutcome::Blocking;
                    }
                    findings = items;
                }
                Err(error) => {
                    step_outcome = warden_core::StepOutcome::Error;
                    findings.push(blocking_finding(
                        role,
                        format!("agent produced invalid findings: {error}"),
                    ));
                }
            }
        }

        if new_commit != commit {
            protect_cycle_commit(&config.repo_path, run_id, cycle_number, &new_commit).await?;
            db::set_cycle_commit_sha(&self.pool, cycle_id, &new_commit).await?;
            if let Some(finding) = agent_definition_tampering_finding(
                worktree_manager,
                run_id,
                &new_commit,
                run_agent_definition_snapshot,
            )
            .await?
            {
                findings.push(finding);
                step_outcome = warden_core::StepOutcome::Blocking;
            }
        }

        if config.workflow.steps[step_index as usize].captures_evidence
            && step_outcome == warden_core::StepOutcome::Clean
        {
            self.capture_evidence_for_cycle(EvidenceCapture {
                run_id,
                cycle_id,
                cycle_number,
                config,
                command: &agent.command,
                worktree_path: worktree.path(),
                cancel,
            })
            .await;
        }
        worktree.remove().await?;
        Ok(StepResult {
            commit: new_commit,
            findings,
            outcome: step_outcome,
        })
    }

    async fn run_command_step(&self, invocation: StepInvocation<'_>) -> Result<StepResult> {
        let StepInvocation {
            run_id,
            cycle_id,
            role,
            worktree_manager,
            commit,
            run,
            cancel,
            ..
        } = invocation;
        let command = run.expect("command steps carry run");
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
        let outcome = if findings.is_empty() {
            warden_core::StepOutcome::Clean
        } else {
            warden_core::StepOutcome::Blocking
        };
        worktree.remove().await?;
        Ok(StepResult {
            commit: commit.to_string(),
            findings,
            outcome,
        })
    }

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
            return Ok(vec![blocking_finding(role, reason)]);
        }
        let sandbox_id = self
            .sandbox
            .create_for_run(warden_sandbox::SandboxSpec { cwd: cwd.into() }, run_id)
            .await?;
        let mut guard = SandboxGuard::new(Arc::clone(&self.sandbox), sandbox_id);
        let result = async {
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
            let output = execution.wait().await?;
            db::mark_agent_process_ended(&self.pool, &process_id, output.exit_code).await?;
            self.publish_event(RunEvent::AgentFinished {
                role: role.as_str().to_string(),
                exit_code: output.exit_code,
                usage: None,
            })
            .await?;
            if output.exit_code == 0 {
                Ok(Vec::new())
            } else {
                Ok(vec![blocking_finding(
                    role,
                    format!("command `{command}` exited {}", output.exit_code),
                )])
            }
        }
        .await;
        if let Err(error) = guard.destroy().await {
            tracing::warn!(cycle_id, role = role.as_str(), %error, "failed to destroy command sandbox");
        }
        result
    }
}
