//! The sandboxed subprocess seam every coder/reviewer/tester invocation
//! runs through: [`Orchestrator::run_agent`], its [`SandboxGuard`]
//! create->destroy pairing (issue #50), and [`map_sandbox_error`].

use super::*;

impl Orchestrator {
    /// Runs `command` through this orchestrator's [`Sandbox`] seam (issue
    /// #50), persisting its PID to `agent_processes` before awaiting
    /// completion so a crash of the orchestrator itself (not the agent) is
    /// still detectable on restart via [`recover_crashed_runs`]. The sandbox
    /// is created bound to `cwd` (the role's own worktree) and destroyed
    /// again once this invocation is done, regardless of outcome --
    /// structurally, via [`SandboxGuard`], not a single `destroy` call on
    /// only the straight-line success path (issue #50 review, MEDIUM 1).
    ///
    /// `stdin_payload` is the serialized `warden_core::AgentInputMessage`
    /// (ADR-0012) fed to the agent's stdin and then closed by the sandbox.
    /// `env_allowlist` is this run's `--tool` adapter's own
    /// `ToolAdapter::env_allowlist` (issue #24), forwarded to
    /// [`warden_sandbox::Command::env_allowlist`] on top of whatever
    /// baseline the backend applies. `runner` (issue #33) translates each
    /// streamed stdout line into a progress detail via
    /// [`ToolAdapter::parse_progress_line`], published live-only through
    /// [`publish_progress_event`](Orchestrator::publish_progress_event)
    /// (never [`publish_event`](Orchestrator::publish_event), which would
    /// persist it -- see this module's own ADR-0008 amendment docs), and is
    /// also asked for the token usage it reported once the invocation
    /// completes (issue #53: [`ToolAdapter::extract_usage`]), persisted onto
    /// this cycle's/the run's running totals and carried on the
    /// `AgentFinished` event this function publishes.
    ///
    /// `repo_path` is the run's base repository; `run_worktrees_root` is
    /// this run's own `<warden_home>/worktrees/<run_id>`. Both are passed
    /// through to [`process::validate_agent_program`] (issue #26), the one
    /// choke point every coder/reviewer/tester spawn goes through, so a
    /// future `ToolAdapter` that names a repo-relative or in-worktree
    /// `command.program` for the reviewer/tester is refused here, before the
    /// sandbox ever runs it.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_agent<R: ToolAdapter>(
        &self,
        cycle_id: &str,
        role: InvocationRole<'_>,
        runner: &R,
        command: &AgentCommand,
        env_allowlist: &[&str],
        cwd: &Path,
        repo_path: &Path,
        run_worktrees_root: &Path,
        stdin_payload: String,
        cancel: CancellationToken,
    ) -> Result<AgentOutcome> {
        // Issue #73: `process::validate_agent_program`'s containment check is
        // role-agnostic beyond "is this the coder" (the one role it never
        // applies to at all) -- a custom step is never the coder, so it gets
        // exactly the same containment check as the reviewer/tester, via the
        // same non-coder branch. `AgentRole::Reviewer` is passed as the
        // *check's own* role argument purely to select that non-coder
        // branch -- it never reaches an error message: on the rare path
        // where this check actually refuses a custom step's program, the
        // `role` field of the resulting `ProcessError::UntrustedAgentProgram`
        // is rewritten below to the step's real, owned role name before it
        // ever surfaces.
        let validate_role = match role {
            InvocationRole::Builtin(role) => role,
            InvocationRole::Custom(_) => AgentRole::Reviewer,
        };
        if let Err(mut error) = process::validate_agent_program(
            validate_role,
            &command.program,
            cwd,
            repo_path,
            run_worktrees_root,
        ) {
            if let InvocationRole::Custom(custom_role) = role {
                if let ProcessError::UntrustedAgentProgram {
                    role: role_field, ..
                } = &mut error
                {
                    *role_field = custom_role.as_str().to_string();
                }
            }
            return Err(error.into());
        }

        let sandbox_id = self
            .sandbox
            .create(warden_sandbox::SandboxSpec {
                cwd: cwd.to_path_buf(),
            })
            .await
            .map_err(map_sandbox_error)?;

        // Issue #50 review, MEDIUM 1: structural create->destroy pairing.
        // Everything below that can exit early via `?` is inside the block
        // this `guard` wraps -- `result` captures its `Ok`/`Err` instead of
        // propagating it directly, so `guard.destroy()` always runs,
        // awaited, right after, whichever way the block ended. See
        // [`SandboxGuard`]'s own docs for why `Drop` is still needed on top,
        // as a backstop for this whole `run_agent` future being dropped
        // mid-`.await` instead of returning normally.
        let mut guard = SandboxGuard::new(Arc::clone(&self.sandbox), sandbox_id);

        let result: Result<AgentOutcome> = async {
                // Issue #33: translates each streamed stdout line into a
                // progress detail (this run's `ToolAdapter`'s own concern --
                // e.g. `claude --output-format stream-json`'s NDJSON events,
                // never a format this module itself understands) and broadcasts
                // it live-only. Must stay synchronous (see
                // `publish_progress_event`'s own docs).
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

                // H1: never persist pid 0. A missing pid right after the sandbox
                // started the process is a typed error, not a silent fallback —
                // a persisted pid 0 would make `is_process_alive` misreport this
                // run as having a live process forever (POSIX `kill(0, ...)`
                // semantics), defeating crash recovery.
                let pid = execution.pid.ok_or_else(|| ProcessError::MissingPid {
                    command: command.program.clone(),
                })?;
                // Issue #73: `agent_processes` (crash-recovery bookkeeping,
                // issue #6) is keyed to the three built-in roles only -- see
                // `InvocationRole`'s own docs on this deliberate scope limit.
                // A custom step's process is still spawned and awaited
                // identically below; it simply gets no `agent_processes` row,
                // so a crash mid-invocation of a custom step is not detected
                // by `recover_crashed_runs` the way a built-in role's is.
                let process_id = if let InvocationRole::Builtin(builtin_role) = role {
                    let process_id = Uuid::new_v4().to_string();
                    db::insert_agent_process(
                        &self.pool,
                        &process_id,
                        cycle_id,
                        builtin_role,
                        pid,
                        &cwd.display().to_string(),
                    )
                    .await?;
                    Some(process_id)
                } else {
                    None
                };
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
                if let Some(process_id) = &process_id {
                    db::mark_agent_process_ended(&self.pool, process_id, exit_code_for_db).await?;
                }

                // L1: log stderr on the success path too — previously only ever
                // surfaced when findings-parsing failed, so a noisy-but-successful
                // agent (warnings, debug chatter) left no trace anywhere.
                if let Ok(outcome) = &outcome_result {
                    if !outcome.stderr.trim().is_empty() {
                        tracing::debug!(cycle_id, ?role, stderr = %outcome.stderr, "agent stderr output");
                    }

                    // Issue #53: grafts onto the exact same captured stdout
                    // `extract_findings` (the caller's own concern, once this
                    // returns) reads -- never a second read of the stream, just a
                    // second, tolerant parse of the buffer already in hand.
                    // `extract_usage` is infallible (`Option`, "n/a" for a tool
                    // that reports nothing) by design -- see its own docs -- so
                    // this never fails an otherwise-successful invocation.
                    let usage = runner.extract_usage(&outcome.stdout);
                    if let Some(usage) = &usage {
                        // Issue #73: the per-cycle *per-role* breakdown is
                        // built-in-roles-only (same `agent_processes` scope
                        // limit as above); the *run-level* running total just
                        // below still accumulates every role's usage,
                        // built-in or custom.
                        if let InvocationRole::Builtin(builtin_role) = role {
                            db::add_cycle_role_token_usage(&self.pool, cycle_id, builtin_role, usage)
                                .await?;
                        }
                        // Only reachable with a run in progress (see
                        // `publish_event`'s own docs on why a missing
                        // `run_context` is a silent no-op here too): a test that
                        // calls `run_agent` directly without going through
                        // `run_convergence_loop` has no run id to attribute a
                        // run-level total to.
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
                }

                outcome_result
            }
            .await;

        // Best-effort: `LocalSandbox::destroy` only ever drops this
        // invocation's own bookkeeping entry (no real OS resource to leak
        // today, but a future `DockerSandbox`'s container very much is one)
        // -- a failure here must never mask the agent's own outcome above,
        // so it's logged, not propagated, the same "cleanup failure is
        // secondary to the outcome already computed" convention this module
        // already uses for worktree removal after a failed coder run.
        if let Err(error) = guard.destroy().await {
            tracing::warn!(cycle_id, ?role, %error, "failed to destroy sandbox after agent invocation");
        }

        result
    }
}

/// RAII guard over one sandbox's `create`->`destroy` lifecycle (issue #50
/// review, MEDIUM 1) -- see [`Orchestrator::run_agent`]'s own docs for why
/// this needs to be structural rather than a single `destroy` call reachable
/// only from the straight-line success path. The common case (still inside
/// `run_agent`'s own future, `Ok` or `Err`) goes through the explicit,
/// awaited [`SandboxGuard::destroy`]. `Drop` is only the backstop for the
/// one path an awaited call can't cover: this whole future being dropped
/// mid-await (run cancellation, `warden run --tui` exit) before
/// [`SandboxGuard::destroy`] resolves -- `id` stays on `self` (never taken
/// out up front) so `Drop` still has it to retry with even if it fires while
/// an explicit `destroy(id).await` is itself in flight (issue #50 review,
/// LOW D).
struct SandboxGuard {
    sandbox: Arc<dyn Sandbox>,
    id: warden_sandbox::SandboxId,
    destroyed: bool,
}

impl SandboxGuard {
    fn new(sandbox: Arc<dyn Sandbox>, id: warden_sandbox::SandboxId) -> Self {
        Self {
            sandbox,
            id,
            destroyed: false,
        }
    }

    /// The id this guard owns.
    fn id(&self) -> &warden_sandbox::SandboxId {
        &self.id
    }

    /// Explicit, awaited teardown for the common (still-inside-`run_agent`'s-
    /// own-future) exit path -- see this type's own docs on why this is
    /// preferred over letting `Drop` handle it whenever the caller can
    /// still `.await`, and on why `destroyed` is only set *after* the
    /// `.await` resolves.
    async fn destroy(&mut self) -> warden_sandbox::Result<()> {
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
        // Backstop only -- see this type's own docs. `Drop` cannot itself
        // `.await`, so the destroy is dispatched onto the ambient tokio
        // runtime instead -- but only if one is actually available (issue
        // #50 review, LOW C): calling `tokio::spawn` with no runtime context
        // panics outright, and a panic while already unwinding from a drop
        // aborts the process. This is a best-effort backstop, not a
        // guarantee: if this drop happens during runtime shutdown (the
        // `warden run --tui` exit case this type's own docs cite), a
        // successfully spawned task can still be cancelled before it runs,
        // silently leaving the sandbox undestroyed -- for `LocalSandbox`
        // that is only an in-memory bookkeeping entry, but a future
        // `DockerSandbox` (#49) container leak here is a real, open
        // limitation of this backstop, not one this guard can close on its
        // own.
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

/// Translates a [`warden_sandbox::SandboxError`] into this crate's own
/// [`ProcessError`] (issue #50): every existing caller/test downstream of
/// [`Orchestrator::run_agent`] -- CLI error text, `assert_cmd` assertions --
/// was written against `ProcessError`'s `Display` output, and a `LocalSandbox`
/// invocation must remain indistinguishable from it (strict parity is this
/// issue's own acceptance criterion). A `SandboxError` variant with no
/// natural `ProcessError` counterpart (only `UnknownSandbox` today -- an
/// internal bug, never expected from a well-behaved backend) still becomes a
/// typed, actionable error rather than a panic or a silently swallowed one.
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
        // Issue #50 review, LOW 6: no `ProcessError` counterpart exists for
        // this one (an internal bug, never expected from a well-behaved
        // backend) -- wrapped via `WardenError`'s own `#[from]` instead of a
        // hand-rolled `reason: String` that would have discarded `#[source]`.
        //
        // Issue #49: `DockerUnavailable` has no `ProcessError` counterpart
        // either -- a docker-specific configuration/precondition failure
        // (a missing `~/.claude`, an unresolvable bind-mount path), not a
        // spawn/wait/stdin-write/cancel shape `LocalSandbox` ever produces.
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

    /// Implements [`Sandbox`] from scratch -- own bookkeeping, own process
    /// spawn, own [`warden_sandbox::Execution`] -- using nothing but this
    /// crate's public API (`SandboxId::new`, `Execution::new`; issue #50
    /// review, MEDIUM A). Deliberately *not* a delegate to [`LocalSandbox`]:
    /// a delegate would only prove `with_sandbox` can carry a wrapper around
    /// the one implementation already in-crate, not that the trait itself is
    /// implementable by an out-of-crate backend, which is exactly what
    /// `DockerSandbox` (#49) will need to be. Records which of
    /// `create`/`execute`/`destroy` ran, in order, and can be told to fail
    /// `execute` outright, to exercise the early-return path `run_agent`'s
    /// own `?` takes right after `execute`.
    struct RecordingSandbox {
        calls: std::sync::Mutex<Vec<&'static str>>,
        cwds: std::sync::Mutex<std::collections::HashMap<warden_sandbox::SandboxId, PathBuf>>,
        fail_execute: bool,
    }

    impl RecordingSandbox {
        fn new(fail_execute: bool) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                cwds: std::sync::Mutex::new(std::collections::HashMap::new()),
                fail_execute,
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl warden_sandbox::Sandbox for RecordingSandbox {
        async fn create(
            &self,
            spec: warden_sandbox::SandboxSpec,
        ) -> warden_sandbox::Result<warden_sandbox::SandboxId> {
            self.calls.lock().unwrap().push("create");
            let id = warden_sandbox::SandboxId::new(uuid::Uuid::new_v4().to_string());
            self.cwds.lock().unwrap().insert(id.clone(), spec.cwd);
            Ok(id)
        }

        async fn execute<'a>(
            &'a self,
            id: &'a warden_sandbox::SandboxId,
            command: warden_sandbox::Command,
            options: warden_sandbox::ExecuteOptions<'a>,
        ) -> warden_sandbox::Result<warden_sandbox::Execution<'a>> {
            self.calls.lock().unwrap().push("execute");
            if self.fail_execute {
                return Err(warden_sandbox::SandboxError::Spawn {
                    program: "recording-sandbox-fixture".to_string(),
                    source: std::io::Error::from(std::io::ErrorKind::NotFound),
                });
            }
            let cwd = self
                .cwds
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .expect("test fixture: execute always called with an id create just returned");

            let mut spawn = tokio::process::Command::new(&command.program);
            spawn
                .args(&command.args)
                .current_dir(&cwd)
                .kill_on_drop(true)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let mut child =
                spawn
                    .spawn()
                    .map_err(|source| warden_sandbox::SandboxError::Spawn {
                        program: command.program.clone(),
                        source,
                    })?;
            let pid = child.id();

            let program = command.program;
            let stdin_payload = command.stdin;
            let cancel = options.cancel;

            Ok(warden_sandbox::Execution::new(pid, async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};

                let mut stdin_handle = child.stdin.take();
                let mut stdout_handle = child.stdout.take();
                let mut stderr_handle = child.stderr.take();

                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        let _ = child.kill().await;
                        Err(warden_sandbox::SandboxError::Cancelled { program })
                    }
                    result = async {
                        let stdin_task = async {
                            if let Some(mut handle) = stdin_handle.take() {
                                if let Some(payload) = stdin_payload {
                                    // A broken pipe here is not a failure --
                                    // it means the child exited without
                                    // reading its payload, which the fake
                                    // `claude` scripts these tests use do
                                    // routinely. `LocalSandbox` classifies it
                                    // the same way (see
                                    // `warden_sandbox::local::classify_stdin_write_error`,
                                    // which logs and continues); propagating
                                    // it instead made this fake diverge from
                                    // the production backend it stands in for,
                                    // and the test fail intermittently with
                                    // `StdinWrite { .. BrokenPipe }` whenever
                                    // the child won the race to exit.
                                    if let Err(error) =
                                        handle.write_all(payload.as_bytes()).await
                                    {
                                        if error.kind() != std::io::ErrorKind::BrokenPipe {
                                            return Err(error);
                                        }
                                    }
                                }
                            }
                            Ok::<(), std::io::Error>(())
                        };
                        let stdout_task = async {
                            let mut buf = Vec::new();
                            if let Some(mut handle) = stdout_handle.take() {
                                handle.read_to_end(&mut buf).await?;
                            }
                            Ok::<Vec<u8>, std::io::Error>(buf)
                        };
                        let stderr_task = async {
                            let mut buf = Vec::new();
                            if let Some(mut handle) = stderr_handle.take() {
                                handle.read_to_end(&mut buf).await?;
                            }
                            Ok::<Vec<u8>, std::io::Error>(buf)
                        };
                        let (stdin_result, stdout_result, stderr_result, status_result) =
                            tokio::join!(stdin_task, stdout_task, stderr_task, child.wait());
                        let status = status_result.map_err(|source| warden_sandbox::SandboxError::Wait {
                            program: program.clone(),
                            source,
                        })?;
                        stdin_result.map_err(|source| warden_sandbox::SandboxError::StdinWrite {
                            program: program.clone(),
                            source,
                        })?;
                        let stdout_buf = stdout_result.map_err(|source| warden_sandbox::SandboxError::Wait {
                            program: program.clone(),
                            source,
                        })?;
                        let stderr_buf = stderr_result.map_err(|source| warden_sandbox::SandboxError::Wait {
                            program: program.clone(),
                            source,
                        })?;
                        Ok(warden_sandbox::ExecutionResult {
                            exit_code: status.code().unwrap_or(-1),
                            stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
                            stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
                        })
                    } => result,
                }
            }))
        }

        async fn destroy(&self, id: warden_sandbox::SandboxId) -> warden_sandbox::Result<()> {
            self.calls.lock().unwrap().push("destroy");
            self.cwds.lock().unwrap().remove(&id);
            Ok(())
        }
    }

    /// Builds an `Orchestrator` wired to `sandbox` via
    /// [`Orchestrator::with_sandbox`], plus the run/cycle rows `run_agent`'s
    /// own `db::insert_agent_process` needs a valid `cycle_id` foreign key
    /// for.
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

    /// `run_agent` must create, execute, and destroy through whatever
    /// backend `with_sandbox` installed -- not always the default
    /// `LocalSandbox` constructed by `Orchestrator::new` -- proving the
    /// seam issue #50 promises is actually reachable.
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

        let outcome = orchestrator
            .run_agent(
                "sandbox-seam-cycle",
                InvocationRole::Builtin(AgentRole::Coder),
                &FakeCommandAdapter,
                &AgentCommand::new("sh", ["-c", "echo hi"]),
                &[],
                dir.path(),
                repo.path(),
                repo.path(),
                "{}".to_string(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout.trim(), "hi");
        assert_eq!(sandbox.calls(), vec!["create", "execute", "destroy"]);
    }

    /// Issue #73 review (LOW cosmetic): `process::validate_agent_program`
    /// refusing a **custom** workflow step's `command.program` (issue #73)
    /// must name that step's own real role in the resulting
    /// `ProcessError::UntrustedAgentProgram` -- not the `AgentRole::Reviewer`
    /// stand-in `run_agent` passes to select the check's non-coder branch.
    #[tokio::test]
    async fn a_custom_steps_containment_violation_names_its_own_real_role_not_the_stand_in() {
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

        // Resolves inside `worktree` itself -- exactly the containment
        // violation `process::validate_agent_program` refuses for any
        // non-coder role.
        let program_inside_worktree = worktree.path().join("evil.sh");
        let techlead_role = Role::new("techlead").unwrap();

        let error = orchestrator
            .run_agent(
                "sandbox-seam-custom-role-cycle",
                InvocationRole::Custom(&techlead_role),
                &FakeCommandAdapter,
                &AgentCommand::new(
                    program_inside_worktree.to_str().unwrap(),
                    Vec::<String>::new(),
                ),
                &[],
                worktree.path(),
                repo.path(),
                run_worktrees_root.path(),
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
            "must not leak the AgentRole::Reviewer stand-in into the error: {rendered}"
        );
    }

    /// Issue #50 review, MEDIUM 1: a sandbox created for an invocation whose
    /// `execute` call itself fails (one of the early-return `?`s `run_agent`
    /// takes right after `create`) must still be destroyed -- not leaked on
    /// the one path that used to skip straight past the single, positional
    /// `destroy` call at the end of the function.
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

        let result = orchestrator
            .run_agent(
                "sandbox-seam-failure-cycle",
                InvocationRole::Builtin(AgentRole::Coder),
                &FakeCommandAdapter,
                &AgentCommand::new("sh", ["-c", "echo hi"]),
                &[],
                dir.path(),
                repo.path(),
                repo.path(),
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

    /// Genuine coverage gap, independently derived from issue #50's own
    /// acceptance criteria (sandbox lifecycle "on cancellation"), distinct
    /// from both neighbours: here the `CancellationToken` passed to
    /// `run_agent` fires while its future keeps running to completion --
    /// never dropped or aborted from outside, unlike
    /// [`sandbox_is_destroyed_when_the_run_agent_future_itself_is_dropped_mid_flight`].
    /// `execution.wait()` resolves on its own with
    /// `SandboxError::Cancelled`, `run_agent`'s `result` plumbing turns that
    /// into `Err(WardenError::Process(ProcessError::Cancelled { .. }))`
    /// (`map_sandbox_error`, strict parity with pre-#50 error text), and the
    /// explicit, awaited `guard.destroy()` right after the inner async block
    /// -- not `SandboxGuard::drop`'s detached backstop -- is what must run.
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

        let result = orchestrator
            .run_agent(
                "sandbox-seam-cancel-cycle",
                InvocationRole::Builtin(AgentRole::Coder),
                &FakeCommandAdapter,
                &AgentCommand::new("sh", ["-c", "sleep 30"]),
                &[],
                dir.path(),
                repo.path(),
                repo.path(),
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

    /// Issue #50 review, MEDIUM 1's other named skip point: the whole
    /// `run_agent` future being dropped mid-`.await` (run cancellation,
    /// `warden run --tui` exit), not just an early `?` return. Aborts the
    /// task running `run_agent` while it's parked on `execution.wait()` (a
    /// long `sleep`, so the abort lands there rather than racing an already-
    /// finished invocation) and asserts `SandboxGuard::drop`'s detached
    /// backstop still destroys the sandbox -- polled for, since that
    /// teardown runs on its own task, not awaited by anything after the
    /// abort.
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
            let _ = orchestrator_for_task
                .run_agent(
                    "sandbox-seam-abort-cycle",
                    InvocationRole::Builtin(AgentRole::Coder),
                    &FakeCommandAdapter,
                    &AgentCommand::new("sh", ["-c", "sleep 30"]),
                    &[],
                    &dir_path,
                    &repo_path,
                    &repo_path,
                    "{}".to_string(),
                    CancellationToken::new(),
                )
                .await;
        });

        // Give the task time to get past `create` and into the long
        // `execution.wait()` await before dropping it mid-flight. Issue #50
        // review, LOW E: this is a best-effort delay, not a synchronization
        // point -- under load the abort can in principle land before
        // `execute` itself has even recorded its call, so what this test
        // asserts below is the property under test (the sandbox created for
        // this invocation is destroyed), not the exact call vector, which a
        // slow scheduler could otherwise make flaky for a reason that has
        // nothing to do with the guard's actual correctness.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        handle.abort();
        let _ = handle.await;

        // `SandboxGuard::drop`'s destroy is dispatched onto a detached
        // task -- poll briefly rather than asserting immediately.
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
