//! [`LocalSandbox`]: strict behavioural parity with the process isolation
//! `warden::process` applied by hand before this issue -- `env_clear()`,
//! `cwd` pointed at the worktree, `kill_on_drop`, stdin/stdout/stderr piped.
//! **No container, on purpose** -- "Local" means exactly what it says: the
//! agent's process runs directly on this host, under this same OS user, the
//! same isolation warden always applied. `DockerSandbox` (#49) is where
//! actual container isolation is added; this type exists so today's default
//! behaviour has a name and a seam to be selected through, not to change
//! what that behaviour is.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::drain::drain_and_wait;
use crate::error::{Result, SandboxError};
use crate::{Command, ExecuteOptions, Execution, Sandbox, SandboxId, SandboxSpec};

/// [`Sandbox`] backed by nothing but [`tokio::process::Command`] -- see this
/// module's own docs on why that is a deliberate parity requirement, not a
/// placeholder. Holds an in-memory `id -> cwd` table rather than any real OS
/// resource: [`LocalSandbox::create`]/[`LocalSandbox::destroy`] are pure
/// bookkeeping (no container to create or tear down), but the seam still
/// requires an id to bind a [`Command`] to *which* worktree it runs against,
/// exactly as a container backend will need one to bind a `docker exec` to
/// *which* container.
#[derive(Default)]
pub struct LocalSandbox {
    sandboxes: Mutex<HashMap<SandboxId, PathBuf>>,
}

impl LocalSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    fn cwd_for(&self, id: &SandboxId) -> Result<PathBuf> {
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| SandboxError::UnknownSandbox { id: id.clone() })
    }
}

#[async_trait]
impl Sandbox for LocalSandbox {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId> {
        let id = SandboxId::generate();
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), spec.cwd);
        Ok(id)
    }

    async fn execute<'a>(
        &'a self,
        id: &'a SandboxId,
        command: Command,
        options: ExecuteOptions<'a>,
    ) -> Result<Execution<'a>> {
        let cwd = self.cwd_for(id)?;

        // Mirrors the now-deleted `warden::process::spawn_with_extra_env`
        // exactly: `env_clear()` always runs first (agents never inherit the
        // orchestrator's own shell environment), `PATH` is always forwarded
        // on top, and `command.env_allowlist` is resolved from *this*
        // process's own environment one variable at a time -- a missing
        // allowlisted var is logged, not fatal (the tool's own error is
        // downstream, e.g. `claude`'s "Not logged in" if `HOME` never made
        // it through).
        let mut cmd = tokio::process::Command::new(&command.program);
        cmd.args(&command.args)
            .current_dir(&cwd)
            .env_clear()
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        for var_name in &command.env_allowlist {
            match std::env::var(var_name) {
                Ok(value) => {
                    cmd.env(var_name, value);
                }
                Err(_) => {
                    tracing::warn!(
                        var = var_name,
                        program = %command.program,
                        "adapter-requested environment variable is not set in warden's own \
                         process environment; the child will run without it"
                    );
                }
            }
        }

        let child = cmd.spawn().map_err(|source| SandboxError::Spawn {
            program: command.program.clone(),
            source,
        })?;
        let pid = child.id();

        let program = command.program;
        let stdin_payload = command.stdin;
        let cancel = options.cancel;
        let on_stdout_line = options.on_stdout_line;

        Ok(Execution::new(
            pid,
            drain_and_wait(child, program, stdin_payload, cancel, on_stdout_line),
        ))
    }

    async fn destroy(&self, id: SandboxId) -> Result<()> {
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn command(program: &str, args: &[&str]) -> Command {
        Command {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env_allowlist: Vec::new(),
            stdin: None,
        }
    }

    #[tokio::test]
    async fn create_then_execute_runs_the_command_in_the_bound_cwd() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(&id, command("pwd", &[]), ExecuteOptions::default())
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            outcome.stdout.trim(),
            dir.path().canonicalize().unwrap().to_str().unwrap()
        );
    }

    #[tokio::test]
    async fn execute_exposes_the_pid_before_completion() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "sleep 0.2"]),
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        assert!(execution.pid.is_some());
        execution.wait().await.unwrap();
    }

    #[tokio::test]
    async fn reports_a_non_zero_exit_code_as_a_normal_outcome_not_an_error() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "exit 7"]),
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();
        assert_eq!(outcome.exit_code, 7);
    }

    #[tokio::test]
    async fn cancellation_kills_the_child_and_returns_cancelled_error() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();
        let cancel = CancellationToken::new();

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "sleep 30"]),
                ExecuteOptions {
                    cancel: cancel.clone(),
                    on_stdout_line: None,
                },
            )
            .await
            .unwrap();
        cancel.cancel();
        let result = execution.wait().await;
        assert!(matches!(result, Err(SandboxError::Cancelled { .. })));
    }

    /// Distinct from [`cancellation_kills_the_child_and_returns_cancelled_error`]:
    /// that test exercises the explicit `cancel.cancel()` path inside
    /// `drain_and_wait`, whose `select!` branch calls `child.kill()` by
    /// hand. This test instead drops the `Execution` (and the `Child` it
    /// owns) mid-flight *without* ever cancelling -- via
    /// `tokio::time::timeout`, whose combinator drops the inner future once
    /// the deadline elapses, the same "future torn down mid-poll rather than
    /// explicitly cancelled" shape `warden::orchestrator`'s `SandboxGuard::drop`
    /// backstop relies on -- isolating `kill_on_drop` itself (set on the
    /// underlying `tokio::process::Command` in [`LocalSandbox::execute`]) as
    /// the thing actually terminating the process, independent of the
    /// cancellation machinery.
    #[tokio::test]
    async fn dropping_the_execution_mid_flight_kills_the_child_via_kill_on_drop_not_the_cancel_path(
    ) {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();
        let marker = dir.path().join("still-alive-after-sleep");

        let execution = sandbox
            .execute(
                &id,
                command(
                    "sh",
                    &["-c", &format!("sleep 1; touch {}", marker.display())],
                ),
                ExecuteOptions::default(),
            )
            .await
            .unwrap();

        // Times out mid the child's own `sleep 1`, well before it would
        // otherwise touch `marker` -- `tokio::time::timeout` drops the inner
        // `execution.wait()` future (and therefore the `Child` it owns) the
        // moment the deadline elapses, without this test ever calling
        // `cancel()` on anything.
        let timed_out =
            tokio::time::timeout(std::time::Duration::from_millis(200), execution.wait())
                .await
                .is_err();
        assert!(
            timed_out,
            "the timeout must fire while the child is still sleeping, for this test to mean \
             anything"
        );

        // Wait well past the child's own `sleep 1` -- if `kill_on_drop` had
        // not fired, the child would keep running on its own and create
        // `marker` regardless of anything on this side having stopped
        // waiting for it.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "kill_on_drop must terminate the child when the Execution future is dropped \
             mid-flight, independent of the explicit cancel-token path"
        );
    }

    #[tokio::test]
    async fn stdin_payload_is_written_and_closed_so_the_child_sees_it_and_exits() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let mut cmd = command("cat", &[]);
        cmd.stdin = Some("hello from warden-sandbox".to_string());
        let execution = sandbox
            .execute(&id, cmd, ExecuteOptions::default())
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "hello from warden-sandbox");
    }

    /// Same regression scenario as
    /// `warden::process::writing_a_large_stdin_payload_does_not_deadlock_on_large_stdout`
    /// -- a large stdin payload and a large stdout write, deliberately
    /// sequenced so a naive "write the whole payload, then read stdout"
    /// implementation would hang.
    #[tokio::test]
    async fn writing_a_large_stdin_payload_does_not_deadlock_on_large_stdout() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let mut cmd = command(
            "sh",
            &["-c", "head -c 200000 /dev/zero; cat > /dev/null; exit 0"],
        );
        cmd.stdin = Some("x".repeat(200_000));

        let execution = sandbox
            .execute(&id, cmd, ExecuteOptions::default())
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution.wait())
            .await
            .expect("execution must not hang when both stdin and stdout exceed the pipe buffer");

        assert_eq!(result.unwrap().exit_code, 0);
    }

    #[tokio::test]
    async fn on_stdout_line_is_invoked_once_per_line_as_it_arrives() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let seen = std::sync::Mutex::new(Vec::new());
        let on_line = |line: &str| seen.lock().unwrap().push(line.to_string());

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "echo one; echo two"]),
                ExecuteOptions {
                    cancel: CancellationToken::new(),
                    on_stdout_line: Some(&on_line),
                },
            )
            .await
            .unwrap();
        execution.wait().await.unwrap();

        assert_eq!(seen.into_inner().unwrap(), vec!["one", "two"]);
    }

    /// Ported from `warden::process`'s own
    /// `wait_with_progress_skips_blank_lines` (issue #50 review, LOW 8):
    /// `process::wait_with_progress`'s callback support is now dead in
    /// production (every remaining call site passes `None`), so this
    /// coverage moved here, the only place the behaviour still runs for
    /// real. Blank lines carry nothing worth surfacing and must not reach
    /// the callback at all.
    #[tokio::test]
    async fn on_stdout_line_skips_blank_lines() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let seen = std::sync::Mutex::new(Vec::new());
        let on_line = |line: &str| seen.lock().unwrap().push(line.to_string());

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "printf 'a\\n\\nb\\n'"]),
                ExecuteOptions {
                    cancel: CancellationToken::new(),
                    on_stdout_line: Some(&on_line),
                },
            )
            .await
            .unwrap();
        execution.wait().await.unwrap();

        assert_eq!(seen.into_inner().unwrap(), vec!["a", "b"]);
    }

    /// Ported from `warden::process`'s own
    /// `wait_with_progress_invokes_the_callback_for_a_final_line_with_no_trailing_newline`
    /// (issue #50 review, LOW 8; see [`on_stdout_line_skips_blank_lines`]'s
    /// own docs on why this moved here). A line with no trailing newline
    /// (the child exits without flushing one, e.g. the very last line of
    /// output) must still reach the callback -- `read_until` returns it on
    /// EOF even without the delimiter.
    #[tokio::test]
    async fn on_stdout_line_is_invoked_for_a_final_line_with_no_trailing_newline() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let seen = std::sync::Mutex::new(Vec::new());
        let on_line = |line: &str| seen.lock().unwrap().push(line.to_string());

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "printf 'no newline at the end'"]),
                ExecuteOptions {
                    cancel: CancellationToken::new(),
                    on_stdout_line: Some(&on_line),
                },
            )
            .await
            .unwrap();
        execution.wait().await.unwrap();

        assert_eq!(seen.into_inner().unwrap(), vec!["no newline at the end"]);
    }

    /// Ported from `warden::process`'s own
    /// `wait_with_progress_does_not_deadlock_on_large_newline_free_stdout`
    /// (issue #50 review, LOW 8; see [`on_stdout_line_skips_blank_lines`]'s
    /// own docs on why this moved here). Same regression scenario as
    /// [`writing_a_large_stdin_payload_does_not_deadlock_on_large_stdout`],
    /// but with an `on_stdout_line` callback attached and stdout that has no
    /// newline until EOF -- proves the line-buffered reader still drains
    /// continuously rather than blocking on a delimiter that never arrives
    /// mid-stream, and that the single oversized "line" is delivered whole,
    /// in one callback invocation, right at EOF.
    #[tokio::test]
    async fn does_not_deadlock_on_large_newline_free_stdout_with_a_callback_attached() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let mut cmd = command(
            "sh",
            &["-c", "head -c 200000 /dev/zero; cat > /dev/null; exit 0"],
        );
        cmd.stdin = Some("x".repeat(200_000));
        let callback_invocations = std::sync::atomic::AtomicUsize::new(0);
        let on_line = |_line: &str| {
            callback_invocations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        };

        let execution = sandbox
            .execute(
                &id,
                cmd,
                ExecuteOptions {
                    cancel: CancellationToken::new(),
                    on_stdout_line: Some(&on_line),
                },
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution.wait())
            .await
            .expect("execution must not hang when stdout has no newlines at all");

        assert_eq!(result.unwrap().exit_code, 0);
        assert_eq!(
            callback_invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the whole newline-free chunk must be delivered as exactly one line, at EOF"
        );
    }

    #[tokio::test]
    async fn an_agent_that_never_reads_stdin_and_exits_immediately_does_not_fail_the_execution() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let mut cmd = command("sh", &["-c", "exit 0"]);
        cmd.stdin = Some("x".repeat(200_000));

        let execution = sandbox
            .execute(&id, cmd, ExecuteOptions::default())
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution.wait())
            .await
            .expect("execution must not hang on a broken pipe");

        let outcome = result.expect("a broken pipe from an agent that ignores stdin must not fail");
        assert_eq!(outcome.exit_code, 0);
    }

    #[tokio::test]
    async fn spawn_of_a_nonexistent_program_reports_a_typed_error() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let result = sandbox
            .execute(
                &id,
                command("this-program-does-not-exist-anywhere", &[]),
                ExecuteOptions::default(),
            )
            .await;
        assert!(matches!(result, Err(SandboxError::Spawn { .. })));
    }

    /// Uses `CARGO_MANIFEST_DIR` -- reliably set by `cargo test` in this
    /// process's own environment, read-only here -- rather than
    /// `std::env::set_var`: mutating global process environment state would
    /// be both `unsafe` and carry cross-test interference risk under a
    /// parallel test runner (same reasoning
    /// `warden::process`'s own `spawn_tui_attach_inherits_the_full_parent_environment`
    /// test documents for the identical trade-off).
    #[tokio::test]
    async fn env_clear_means_an_unallowlisted_variable_never_reaches_the_child() {
        assert!(
            std::env::var("CARGO_MANIFEST_DIR").is_ok(),
            "precondition: cargo test sets CARGO_MANIFEST_DIR"
        );
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "echo \"[$CARGO_MANIFEST_DIR]\""]),
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert_eq!(outcome.stdout.trim(), "[]");
    }

    #[tokio::test]
    async fn env_allowlist_forwards_only_the_named_variables() {
        let expected = std::env::var("CARGO_MANIFEST_DIR")
            .expect("precondition: cargo test sets CARGO_MANIFEST_DIR");
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let mut cmd = command("sh", &["-c", "echo \"[$CARGO_MANIFEST_DIR]\""]);
        cmd.env_allowlist = vec!["CARGO_MANIFEST_DIR".to_string()];

        let execution = sandbox
            .execute(&id, cmd, ExecuteOptions::default())
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert_eq!(outcome.stdout.trim(), format!("[{expected}]"));
    }

    #[tokio::test]
    async fn execute_with_an_unknown_sandbox_id_reports_a_typed_error() {
        let sandbox = LocalSandbox::new();
        let bogus_id = SandboxId::generate();

        let result = sandbox
            .execute(&bogus_id, command("true", &[]), ExecuteOptions::default())
            .await;
        assert!(matches!(result, Err(SandboxError::UnknownSandbox { .. })));
    }

    #[tokio::test]
    async fn destroy_is_idempotent_for_an_id_that_was_never_created() {
        let sandbox = LocalSandbox::new();
        assert!(sandbox.destroy(SandboxId::generate()).await.is_ok());
    }

    #[tokio::test]
    async fn destroy_then_execute_reports_unknown_sandbox() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();
        sandbox.destroy(id.clone()).await.unwrap();

        let result = sandbox
            .execute(&id, command("true", &[]), ExecuteOptions::default())
            .await;
        assert!(matches!(result, Err(SandboxError::UnknownSandbox { .. })));
    }
}
