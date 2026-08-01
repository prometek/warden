use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use tokio::process::Command;
use warden_core::EvidenceRow;

use crate::error::{Result, WardenError};

/// A handle to a triggered `warden-gated` subprocess that resolves once the child has exited.
pub struct GateChild {
    exited: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl GateChild {
    fn new(exited: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            exited: Box::pin(exited),
        }
    }

    /// Resolves once the triggered subprocess has exited (or is otherwise known to be gone).
    pub async fn wait_exit(self) {
        self.exited.await
    }

    /// Test helper: a child that never exits on its own -- models a live `warden-gated` still
    /// watching CI.
    #[cfg(test)]
    pub fn never_exiting() -> Self {
        Self::new(std::future::pending())
    }

    /// Test helper: a child that has already exited -- models a `warden-gated` that returned before
    /// delivering anything (a hard early failure).
    #[cfg(test)]
    pub fn already_exited() -> Self {
        Self::new(std::future::ready(()))
    }
}

/// Everything a fresh (first-time) tail trigger needs to pass to `warden-gated run-tail`.
pub struct RunTailTrigger<'a> {
    pub run_id: &'a str,
    pub branch: &'a str,
    pub base_branch: &'a str,
    pub intent: &'a str,
    pub pushed_commit_sha: &'a str,
    /// The PR body's summary text -- delivered to `run-tail` over its stdin, never as a CLI
    /// argument (arbitrary length/escaping).
    pub summary_body: &'a str,
    pub ci_result_socket: &'a Path,
    /// Evidence captured across this run's cycles -- folded into the finalized PR body's Evidence
    /// section when non-empty.
    pub evidence: &'a [EvidenceRow],
    /// The PR already opened for this run in an earlier attempt: `Some` on a reboucle (this run has
    /// already been through this tail once), `None` only on a run's first pass.
    pub existing_pr_number: Option<u64>,
}

/// Requests `warden-gated` to (re)start a run's post-`Converged` tail.
#[allow(async_fn_in_trait)]
pub trait GateTrigger {
    /// Starts the fresh tail: skeleton commit + `OpenDraft` + `Finalize` + `watch_pr`.
    async fn trigger_run_tail(&self, request: &RunTailTrigger<'_>) -> Result<GateChild>;

    /// Resumes watching an already-opened, already-finalized PR: `OpenDraft`/`Finalize` are not
    /// repeated.
    async fn trigger_resume_watch(
        &self,
        run_id: &str,
        pr_number: u64,
        ci_result_socket: &Path,
    ) -> Result<GateChild>;
}

/// The production [`GateTrigger`]: spawns `warden-gated run-tail`/ `resume-watch` as a child
/// process and returns once it has spawned successfully, without waiting for it to exit.
pub struct SubprocessGateTrigger {
    pub gated_bin: PathBuf,
    /// `warden`'s own SQLite database -- passed through so `warden-gated` can open it read-only and
    /// independently re-verify the run itself (never trusted from `warden`'s own say-so).
    pub db_path: PathBuf,
    pub bare_repo_path: PathBuf,
    pub repo_slug: Option<String>,
    pub poll_interval_secs: u64,
    pub inactivity_timeout_secs: u64,
}

impl GateTrigger for SubprocessGateTrigger {
    async fn trigger_run_tail(&self, request: &RunTailTrigger<'_>) -> Result<GateChild> {
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
        if !request.evidence.is_empty() {
            let evidence_json = warden_core::serialize_evidence_rows(request.evidence)?;
            command.arg("--evidence-json").arg(evidence_json);
        }
        if let Some(pr_number) = request.existing_pr_number {
            command
                .arg("--existing-pr-number")
                .arg(pr_number.to_string());
        }
        spawn_watched_with_stdin(command, request.summary_body).await
    }

    async fn trigger_resume_watch(
        &self,
        run_id: &str,
        _pr_number: u64,
        ci_result_socket: &Path,
    ) -> Result<GateChild> {
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
        spawn_watched(command).await
    }
}

async fn spawn_watched_with_stdin(mut command: Command, stdin: &str) -> Result<GateChild> {
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
    Ok(watch_child_exit(child, "run-tail"))
}

/// Spawns `command` (no stdin) and returns a [`GateChild`] tracking its exit.
async fn spawn_watched(mut command: Command) -> Result<GateChild> {
    let debug_command = format!("{command:?}");
    let child = command.spawn().map_err(|source| {
        WardenError::Process(crate::error::ProcessError::Spawn {
            command: debug_command,
            source,
        })
    })?;
    Ok(watch_child_exit(child, "resume-watch"))
}

fn watch_child_exit(mut child: tokio::process::Child, subcommand: &'static str) -> GateChild {
    let (exited_tx, exited_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) if !status.success() => {
                tracing::warn!(
                    ?status,
                    subcommand,
                    "warden-gated subprocess exited non-zero"
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, subcommand, "failed to wait on warden-gated subprocess");
            }
        }
        let _ = exited_tx.send(());
    });
    // If the waiter task is ever cancelled, `exited_tx` drops and the awaited `Err(RecvError)`
    // still counts as "the child is gone" -- never a hang.
    GateChild::new(async move {
        let _ = exited_rx.await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
                evidence: &[],
                existing_pr_number: None,
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
