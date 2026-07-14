//! Abstracts "ask `warden-gated` to run/resume this run's post-`Converged`
//! tail" (issue #15/ADR-0011) behind a trait, so the orchestrator's own
//! tests can inject a fake trigger instead of spawning a real
//! `warden-gated` subprocess -- which would need a real `gh`/GitHub PR to
//! talk to (code-standards.md: "pas d'appel réseau externe" in tests).
//! [`SubprocessGateTrigger`] is the real, production implementation.
//!
//! `warden` still never touches `origin`/PR credentials itself (ADR-0006):
//! it only execs the separately privileged `warden-gated` binary, which
//! independently re-verifies the run against its own read-only view of
//! SQLite (`Finalize`'s `verify_and_authorize`, `resume-watch`'s
//! `get_awaiting_ci_run_view`) before doing anything -- the same trust
//! boundary `warden` already crosses spawning `git`/agent CLIs (ADR-0005).

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::error::{Result, WardenError};

/// Everything a fresh (first-time) tail trigger needs to pass to
/// `warden-gated run-tail`.
pub struct RunTailTrigger<'a> {
    pub run_id: &'a str,
    pub branch: &'a str,
    pub base_branch: &'a str,
    pub intent: &'a str,
    pub pushed_commit_sha: &'a str,
    /// The PR body's summary text -- delivered to `run-tail` over its
    /// stdin, never as a CLI argument (arbitrary length/escaping).
    pub summary_body: &'a str,
    pub ci_result_socket: &'a Path,
}

/// Requests `warden-gated` to (re)start a run's post-`Converged` tail
/// (ADR-0011: "`warden` possède le déclencheur du watch"). Both methods
/// return once the request has been successfully *issued*, not once the
/// tail has completed -- the eventual terminal outcome arrives later, over
/// `ci_result_socket`, delivered by `warden-gated` itself.
#[allow(async_fn_in_trait)]
pub trait GateTrigger {
    /// Starts the fresh tail: skeleton commit + `OpenDraft` + `Finalize` +
    /// `watch_pr`. Triggered once, on first entering `AwaitingCi`.
    async fn trigger_run_tail(&self, request: &RunTailTrigger<'_>) -> Result<()>;

    /// Resumes watching an already-opened, already-finalized PR (Phase 6
    /// crash recovery, ADR-0011): `OpenDraft`/`Finalize` are not repeated.
    async fn trigger_resume_watch(
        &self,
        run_id: &str,
        pr_number: u64,
        ci_result_socket: &Path,
    ) -> Result<()>;
}

/// The production [`GateTrigger`]: spawns `warden-gated run-tail`/
/// `resume-watch` as a child process (ADR-0005 precedent -- `warden` already
/// spawns `git` and agent CLIs) and returns once it has spawned
/// successfully, without waiting for it to exit. A spawn failure (binary
/// missing, bad args) surfaces immediately as a typed error; a failure
/// *during* the child's own run is instead reported later, as a
/// `CiWatchOutcome::GateFailed` delivered over the reverse socket (the
/// child's whole job, per `warden_gated::run_tail`'s docs, is to always
/// produce that terminal message rather than just exit non-zero silently).
pub struct SubprocessGateTrigger {
    pub gated_bin: PathBuf,
    /// `warden`'s own SQLite database -- passed through so `warden-gated`
    /// can open it read-only and independently re-verify the run itself
    /// (never trusted from `warden`'s own say-so).
    pub db_path: PathBuf,
    pub bare_repo_path: PathBuf,
    pub repo_slug: Option<String>,
    pub poll_interval_secs: u64,
    pub inactivity_timeout_secs: u64,
}

impl GateTrigger for SubprocessGateTrigger {
    async fn trigger_run_tail(&self, request: &RunTailTrigger<'_>) -> Result<()> {
        let mut command = Command::new(&self.gated_bin);
        command
            .arg("run-tail")
            .arg("--run-id")
            .arg(request.run_id)
            .arg("--db")
            .arg(&self.db_path)
            .arg("--bare-repo")
            .arg(&self.bare_repo_path)
            .arg("--branch")
            .arg(request.branch)
            .arg("--base-branch")
            .arg(request.base_branch)
            .arg("--intent")
            .arg(request.intent)
            .arg("--pushed-commit")
            .arg(request.pushed_commit_sha)
            .arg("--ci-result-socket")
            .arg(request.ci_result_socket)
            .arg("--poll-interval-secs")
            .arg(self.poll_interval_secs.to_string())
            .arg("--inactivity-timeout-secs")
            .arg(self.inactivity_timeout_secs.to_string());
        if let Some(repo_slug) = &self.repo_slug {
            command.arg("--repo").arg(repo_slug);
        }
        spawn_detached_with_stdin(command, request.summary_body).await
    }

    async fn trigger_resume_watch(
        &self,
        run_id: &str,
        _pr_number: u64,
        ci_result_socket: &Path,
    ) -> Result<()> {
        // `warden-gated resume-watch` re-derives the PR number itself from
        // `runs.pr_number` (never trusting the caller's copy) -- `pr_number`
        // is still part of this trait's signature so a fake `GateTrigger`
        // used in tests can assert on it without needing its own DB read.
        let mut command = Command::new(&self.gated_bin);
        command
            .arg("resume-watch")
            .arg("--run-id")
            .arg(run_id)
            .arg("--db")
            .arg(&self.db_path)
            .arg("--bare-repo")
            .arg(&self.bare_repo_path)
            .arg("--ci-result-socket")
            .arg(ci_result_socket)
            .arg("--poll-interval-secs")
            .arg(self.poll_interval_secs.to_string())
            .arg("--inactivity-timeout-secs")
            .arg(self.inactivity_timeout_secs.to_string());
        if let Some(repo_slug) = &self.repo_slug {
            command.arg("--repo").arg(repo_slug);
        }
        spawn_detached(command).await
    }
}

/// Spawns `command` with `stdin` piped and immediately written/closed, then
/// detaches (never awaits the child's exit -- see this module's docs on why).
async fn spawn_detached_with_stdin(mut command: Command, stdin: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    command.stdin(std::process::Stdio::piped());
    let debug_command = format!("{command:?}");
    let mut child = command.spawn().map_err(|source| {
        WardenError::Process(crate::error::ProcessError::Spawn {
            command: debug_command,
            source,
        })
    })?;
    if let Some(mut child_stdin) = child.stdin.take() {
        child_stdin.write_all(stdin.as_bytes()).await?;
    }
    tokio::spawn(async move {
        if let Ok(status) = child.wait().await {
            if !status.success() {
                tracing::warn!(?status, "warden-gated run-tail subprocess exited non-zero");
            }
        }
    });
    Ok(())
}

/// Spawns `command` and detaches, without any stdin to write.
async fn spawn_detached(mut command: Command) -> Result<()> {
    let debug_command = format!("{command:?}");
    let mut child = command.spawn().map_err(|source| {
        WardenError::Process(crate::error::ProcessError::Spawn {
            command: debug_command,
            source,
        })
    })?;
    tokio::spawn(async move {
        if let Ok(status) = child.wait().await {
            if !status.success() {
                tracing::warn!(
                    ?status,
                    "warden-gated resume-watch subprocess exited non-zero"
                );
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A spawn failure (the binary doesn't exist) must surface as a typed
    /// error immediately, not be swallowed or panic -- `trigger_run_tail`'s
    /// whole contract is "return once the request has been *issued*", and
    /// an unspawnable binary means it never was.
    #[tokio::test]
    async fn trigger_run_tail_surfaces_a_spawn_failure_as_a_typed_error() {
        let dir = TempDir::new().unwrap();
        let trigger = SubprocessGateTrigger {
            gated_bin: dir.path().join("does-not-exist-binary"),
            db_path: dir.path().join("state.db"),
            bare_repo_path: dir.path().join("bare.git"),
            repo_slug: None,
            poll_interval_secs: 15,
            inactivity_timeout_secs: 1800,
        };
        let socket_path = dir.path().join("run-1.ci.sock");

        let result = trigger
            .trigger_run_tail(&RunTailTrigger {
                run_id: "run-1",
                branch: "warden/run-1",
                base_branch: "main",
                intent: "do the thing",
                pushed_commit_sha: "deadbeef",
                summary_body: "summary",
                ci_result_socket: &socket_path,
            })
            .await;

        assert!(matches!(result, Err(WardenError::Process(_))));
    }

    #[tokio::test]
    async fn trigger_resume_watch_surfaces_a_spawn_failure_as_a_typed_error() {
        let dir = TempDir::new().unwrap();
        let trigger = SubprocessGateTrigger {
            gated_bin: dir.path().join("does-not-exist-binary"),
            db_path: dir.path().join("state.db"),
            bare_repo_path: dir.path().join("bare.git"),
            repo_slug: None,
            poll_interval_secs: 15,
            inactivity_timeout_secs: 1800,
        };
        let socket_path = dir.path().join("run-1.ci.sock");

        let result = trigger
            .trigger_resume_watch("run-1", 42, &socket_path)
            .await;

        assert!(matches!(result, Err(WardenError::Process(_))));
    }
}
