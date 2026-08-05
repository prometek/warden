//! The sandboxed subprocess seam every workflow step's invocation runs through:
//! [`Orchestrator::run_agent`], its [`SandboxGuard`] create->destroy pairing, and
//! [`map_sandbox_error`].

use super::*;

impl Orchestrator {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_agent<R: ToolAdapter>(
        &self,
        cycle_id: &str,
        role: &Role,
        runner: &R,
        command: &AgentCommand,
        env_allowlist: &[&str],
        cwd: &Path,
        repo_path: &Path,
        run_worktrees_root: &Path,
        trusted_arg_values: &[String],
        stdin_payload: String,
        cancel: CancellationToken,
    ) -> Result<AgentOutcome> {
        process::validate_agent_program(
            role.as_str(),
            &command.program,
            &command.args,
            cwd,
            repo_path,
            run_worktrees_root,
            trusted_arg_values,
        )?;

        let sandbox_spec = warden_sandbox::SandboxSpec {
            cwd: cwd.to_path_buf(),
        };
        let sandbox_id = match self.run_context.get() {
            Some(context) => {
                self.sandbox
                    .create_for_run(sandbox_spec, &context.run_id)
                    .await
            }
            None => self.sandbox.create(sandbox_spec).await,
        }
        .map_err(map_sandbox_error)?;

        let mut guard = SandboxGuard::new(Arc::clone(&self.sandbox), sandbox_id);

        // Opens this invocation's persisted-progress budget before a single line can be read. The
        // convergence loop re-enters `run_agent` for the same step on every reboucle, so each entry
        // gets its own budget.
        self.begin_progress_invocation();

        let result: Result<AgentOutcome> = async {
                let on_stdout_line = |line: &str| {
                    if let Some(detail) = runner.parse_progress_line(line) {
                        self.publish_progress_event(role.as_str(), detail);
                    }
                };
                let sandbox_command = warden_sandbox::Command {
                    program: command.program.clone(),
                    args: command.args.clone(),
                    env_allowlist: env_allowlist.iter().map(|name| name.to_string()).collect(),
                    stdin: Some(stdin_payload),
                };
                let execution = self
                    .sandbox
                    .execute(
                        guard.id(),
                        sandbox_command,
                        warden_sandbox::ExecuteOptions {
                            cancel,
                            on_stdout_line: Some(&on_stdout_line),
                        },
                    )
                    .await
                    .map_err(map_sandbox_error)?;

                let pid = execution.pid.ok_or_else(|| ProcessError::MissingPid {
                    command: command.program.clone(),
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

                let outcome_result = execution
                    .wait()
                    .await
                    .map(|result| AgentOutcome {
                        exit_code: result.exit_code,
                        stdout: result.stdout,
                        stderr: result.stderr,
                    })
                    .map_err(map_sandbox_error);
                // `wait` has returned, so every `on_stdout_line` callback has already fired: drain
                // this invocation's queued progress *before* `AgentFinished` is persisted below, so
                // replay order matches publication order. Infallible -- a write failure here never
                // touches the run's outcome.
                self.flush_progress().await;
                let exit_code_for_db = match &outcome_result {
                    Ok(outcome) => outcome.exit_code,
                    Err(_) => -1,
                };
                db::mark_agent_process_ended(&self.pool, &process_id, exit_code_for_db).await?;

                if let Ok(outcome) = &outcome_result {
                    if !outcome.stderr.trim().is_empty() {
                        tracing::debug!(cycle_id, ?role, stderr = %outcome.stderr, "agent stderr output");
                    }

                    let usage = runner.extract_usage(&outcome.stdout);
                    if let Some(usage) = &usage {
                        db::add_cycle_role_token_usage(&self.pool, cycle_id, role.as_str(), usage)
                            .await?;
                        if let Some(context) = self.run_context.get() {
                            db::add_run_token_usage(&self.pool, &context.run_id, usage).await?;
                        }
                    }

                    self.publish_event(RunEvent::AgentFinished {
                        role: role.as_str().to_string(),
                        exit_code: outcome.exit_code,
                        usage,
                    })
                    .await?;

                    // same seam-grafting convention as `extract_usage` just above -- reads the
                    // exact same captured stdout, no second stream read.
                    if let Some(rate_limit) = runner.extract_rate_limit(&outcome.stdout) {
                        if let Some(context) = self.run_context.get() {
                            db::set_run_rate_limit_status(&self.pool, &context.run_id, &rate_limit)
                                .await?;
                        }
                        self.publish_event(RunEvent::RateLimitStatusUpdated {
                            role: role.as_str().to_string(),
                            status: rate_limit,
                        })
                        .await?;
                    }
                }

                outcome_result
            }
            .await;

        if let Err(error) = guard.destroy().await {
            tracing::warn!(cycle_id, ?role, %error, "failed to destroy sandbox after agent invocation");
        }

        result
    }
}

pub(super) struct SandboxGuard {
    sandbox: Arc<dyn Sandbox>,
    id: warden_sandbox::SandboxId,
    destroyed: bool,
}

impl SandboxGuard {
    pub(super) fn new(sandbox: Arc<dyn Sandbox>, id: warden_sandbox::SandboxId) -> Self {
        Self {
            sandbox,
            id,
            destroyed: false,
        }
    }

    /// The id this guard owns.
    pub(super) fn id(&self) -> &warden_sandbox::SandboxId {
        &self.id
    }

    pub(super) async fn destroy(&mut self) -> warden_sandbox::Result<()> {
        if self.destroyed {
            return Ok(());
        }
        let result = self.sandbox.destroy(self.id.clone()).await;
        self.destroyed = true;
        result
    }
}

impl Drop for SandboxGuard {
    fn drop(&mut self) {
        if self.destroyed {
            return;
        }
        self.destroyed = true;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let sandbox = Arc::clone(&self.sandbox);
                let id = self.id.clone();
                handle.spawn(async move {
                    if let Err(error) = sandbox.destroy(id).await {
                        tracing::warn!(%error, "failed to destroy sandbox during drop cleanup");
                    }
                });
            }
            Err(_) => {
                tracing::warn!(
                    id = %self.id,
                    "sandbox guard dropped with no tokio runtime available to dispatch \
                     teardown onto; sandbox left undestroyed"
                );
            }
        }
    }
}

fn map_sandbox_error(error: warden_sandbox::SandboxError) -> WardenError {
    use warden_sandbox::SandboxError;
    match error {
        SandboxError::Spawn { program, source } => ProcessError::Spawn {
            command: program,
            source,
        }
        .into(),
        SandboxError::Cancelled { program } => ProcessError::Cancelled { command: program }.into(),
        SandboxError::Wait { program, source } => ProcessError::Wait {
            command: program,
            source,
        }
        .into(),
        SandboxError::StdinWrite { program, source } => ProcessError::StdinWrite {
            command: program,
            source,
        }
        .into(),
        error @ (SandboxError::UnknownSandbox { .. } | SandboxError::DockerUnavailable { .. }) => {
            WardenError::Sandbox(error)
        }
    }
}
