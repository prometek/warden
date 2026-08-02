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
        is_producer: bool,
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
            is_producer,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::test_support::*;
    use tempfile::TempDir;

    async fn orchestrator_with_sandbox_and_cycle(
        pool: &SqlitePool,
        sandbox: Arc<dyn Sandbox>,
        run_id: &str,
        cycle_id: &str,
    ) -> Orchestrator {
        db::insert_run(pool, run_id, "/tmp/repo", "main", "intent", 3, 3, 3, 5)
            .await
            .unwrap();
        db::insert_cycle(pool, cycle_id, run_id, 1).await.unwrap();
        Orchestrator::new(pool.clone()).with_sandbox(sandbox)
    }

    #[tokio::test]
    async fn with_sandbox_installs_a_custom_backend_and_routes_run_agent_through_it() {
        let dir = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let sandbox = Arc::new(RecordingSandbox::new(false));

        let orchestrator = orchestrator_with_sandbox_and_cycle(
            &pool,
            sandbox.clone() as Arc<dyn Sandbox>,
            "sandbox-seam-run",
            "sandbox-seam-cycle",
        )
        .await;

        let coder_role = Role::new("coder").unwrap();
        let outcome = orchestrator
            .run_agent(
                "sandbox-seam-cycle",
                &coder_role,
                true,
                &FakeCommandAdapter,
                &AgentCommand::new("sh", ["-c", "echo hi"]),
                &[],
                dir.path(),
                repo.path(),
                repo.path(),
                &[],
                "{}".to_string(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout.trim(), "hi");
        assert_eq!(sandbox.calls(), vec!["create", "execute", "destroy"]);
    }

    #[tokio::test]
    async fn a_custom_steps_containment_violation_names_its_own_real_role() {
        let worktree = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let run_worktrees_root = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let sandbox = Arc::new(RecordingSandbox::new(false));

        let orchestrator = orchestrator_with_sandbox_and_cycle(
            &pool,
            sandbox as Arc<dyn Sandbox>,
            "sandbox-seam-custom-role-run",
            "sandbox-seam-custom-role-cycle",
        )
        .await;

        let program_inside_worktree = worktree.path().join("evil.sh");
        let techlead_role = Role::new("techlead").unwrap();

        let error = orchestrator
            .run_agent(
                "sandbox-seam-custom-role-cycle",
                &techlead_role,
                false,
                &FakeCommandAdapter,
                &AgentCommand::new(
                    program_inside_worktree.to_str().unwrap(),
                    Vec::<String>::new(),
                ),
                &[],
                worktree.path(),
                repo.path(),
                run_worktrees_root.path(),
                &[],
                "{}".to_string(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        let rendered = error.to_string();
        assert!(
            rendered.contains("techlead"),
            "expected the real custom role name in the error, got: {rendered}"
        );
        assert!(
            !rendered.contains("reviewer"),
            "must never name an unrelated built-in role: {rendered}"
        );
    }

    #[tokio::test]
    async fn sandbox_is_destroyed_even_when_execute_fails() {
        let dir = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let sandbox = Arc::new(RecordingSandbox::new(true));

        let orchestrator = orchestrator_with_sandbox_and_cycle(
            &pool,
            sandbox.clone() as Arc<dyn Sandbox>,
            "sandbox-seam-failure-run",
            "sandbox-seam-failure-cycle",
        )
        .await;

        let coder_role = Role::new("coder").unwrap();
        let result = orchestrator
            .run_agent(
                "sandbox-seam-failure-cycle",
                &coder_role,
                true,
                &FakeCommandAdapter,
                &AgentCommand::new("sh", ["-c", "echo hi"]),
                &[],
                dir.path(),
                repo.path(),
                repo.path(),
                &[],
                "{}".to_string(),
                CancellationToken::new(),
            )
            .await;

        assert!(
            result.is_err(),
            "a failing execute must fail the invocation"
        );
        assert_eq!(
            sandbox.calls(),
            vec!["create", "execute", "destroy"],
            "the sandbox created before the failing `execute` call must still be destroyed"
        );
    }

    #[tokio::test]
    async fn sandbox_is_destroyed_when_cancellation_resolves_the_future_normally() {
        let dir = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let sandbox = Arc::new(RecordingSandbox::new(false));

        let orchestrator = orchestrator_with_sandbox_and_cycle(
            &pool,
            sandbox.clone() as Arc<dyn Sandbox>,
            "sandbox-seam-cancel-run",
            "sandbox-seam-cancel-cycle",
        )
        .await;

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        let coder_role = Role::new("coder").unwrap();
        let result = orchestrator
            .run_agent(
                "sandbox-seam-cancel-cycle",
                &coder_role,
                true,
                &FakeCommandAdapter,
                &AgentCommand::new("sh", ["-c", "sleep 30"]),
                &[],
                dir.path(),
                repo.path(),
                repo.path(),
                &[],
                "{}".to_string(),
                cancel,
            )
            .await;

        assert!(
            matches!(
                result,
                Err(WardenError::Process(ProcessError::Cancelled { .. }))
            ),
            "a cancelled agent must surface as ProcessError::Cancelled (strict parity with \
                 pre-#50 behaviour), got {result:?}"
        );
        assert_eq!(
            sandbox.calls(),
            vec!["create", "execute", "destroy"],
            "destroy must run via the explicit, awaited call in `run_agent` -- not just \
                 `SandboxGuard::drop`'s backstop -- when cancellation resolves the future \
                 normally rather than the future being dropped/aborted from outside"
        );
    }

    #[tokio::test]
    async fn sandbox_is_destroyed_when_the_run_agent_future_itself_is_dropped_mid_flight() {
        let dir = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let sandbox = Arc::new(RecordingSandbox::new(false));

        let orchestrator = Arc::new(
            orchestrator_with_sandbox_and_cycle(
                &pool,
                sandbox.clone() as Arc<dyn Sandbox>,
                "sandbox-seam-abort-run",
                "sandbox-seam-abort-cycle",
            )
            .await,
        );
        let orchestrator_for_task = Arc::clone(&orchestrator);
        let dir_path = dir.path().to_path_buf();
        let repo_path = repo.path().to_path_buf();

        let handle = tokio::spawn(async move {
            let coder_role = Role::new("coder").unwrap();
            let _ = orchestrator_for_task
                .run_agent(
                    "sandbox-seam-abort-cycle",
                    &coder_role,
                    true,
                    &FakeCommandAdapter,
                    &AgentCommand::new("sh", ["-c", "sleep 30"]),
                    &[],
                    &dir_path,
                    &repo_path,
                    &repo_path,
                    &[],
                    "{}".to_string(),
                    CancellationToken::new(),
                )
                .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        handle.abort();
        let _ = handle.await;

        for _ in 0..200 {
            if sandbox.calls().contains(&"destroy") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let calls = sandbox.calls();
        assert!(
            calls.contains(&"create"),
            "expected the sandbox to have been created before the abort, got {calls:?}"
        );
        assert!(
            calls.contains(&"destroy"),
            "expected `SandboxGuard::drop`'s backstop to destroy the sandbox created for a \
                 future dropped mid-flight, got {calls:?}"
        );
    }
}
