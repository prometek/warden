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
//!
//! **Exception to code-standards.md's "Inter-process Communication" rule**
//! ("warden -> warden-gated : ... Aucun autre canal de commande entre les
//! deux" -- no channel besides the git-push+hook one) and to ADR-0011's own
//! phrasing ("`warden-gated` démarre `watch_pr` ... au `Finalize`, qu'il
//! traite déjà via le hook `post-receive`"), justified here per
//! code-standards.md's own "toute exception doit être justifiée en
//! commentaire": the git-push+hook channel is one-directional and
//! content-triggered -- a hook only fires on a genuine ref *update* (a new
//! old-sha/new-sha pair). The first `run-tail` trigger *does* have new
//! content (the just-converged commit) and could in principle ride that
//! channel, but the Phase 6 crash-recovery `resume-watch` trigger has
//! nothing new to push at all -- `warden` restarted, the run's content is
//! unchanged, and forcing a spurious ref update just to get a hook to fire
//! would be indistinguishable from (and easily confusable with) a genuine
//! new push. Subprocess invocation is used for *both* triggers instead, for
//! one consistent mechanism rather than two: it does not weaken the
//! security boundary those rules protect (`warden` still never touches
//! `origin`/PR credentials; `warden-gated` still independently re-verifies
//! from its own read-only SQLite view before doing anything, exactly as it
//! already does for the git-push+hook path), it only changes *how* the
//! request reaches an already-untrusted-by-default `warden-gated`. See
//! `docs/Architecture.md`'s ADR-0011 amendment for the recorded decision.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use tokio::process::Command;
use warden_core::EvidenceRow;

use crate::error::{Result, WardenError};

/// A handle to a triggered `warden-gated` subprocess that resolves once the
/// child has exited (issue #15 review, M-new-1). The orchestrator selects on
/// this alongside `CiResultListener::receive` so its wait for the terminal CI
/// result is bounded by the child's *liveness* rather than a wall-clock guess:
/// a wall-clock bound derived from `watch_pr`'s inactivity timeout is wrong,
/// because `watch_pr` has no absolute cap (its inactivity clock resets on every
/// status change), so a long-but-active CI would be spuriously failed. A live
/// child means "keep waiting" (the watch is still progressing, bounded by its
/// own inactivity timeout on the gated side); a child that exits *without*
/// having delivered a message means the run must be failed rather than waited
/// on forever.
pub struct GateChild {
    exited: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl GateChild {
    fn new(exited: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            exited: Box::pin(exited),
        }
    }

    /// Resolves once the triggered subprocess has exited (or is otherwise
    /// known to be gone). Consumes the handle -- it is awaited exactly once,
    /// in the orchestrator's `select!`.
    pub async fn wait_exit(self) {
        self.exited.await
    }

    /// Test helper: a child that never exits on its own -- models a live
    /// `warden-gated` still watching CI. The orchestrator must keep waiting
    /// for the delivered message rather than failing the run.
    #[cfg(test)]
    pub fn never_exiting() -> Self {
        Self::new(std::future::pending())
    }

    /// Test helper: a child that has already exited -- models a `warden-gated`
    /// that returned before delivering anything (a hard early failure). The
    /// orchestrator must fail the run once its grace period elapses.
    #[cfg(test)]
    pub fn already_exited() -> Self {
        Self::new(std::future::ready(()))
    }
}

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
    /// Evidence captured across this run's cycles (issue #15 review, M2) --
    /// folded into the finalized PR body's Evidence section (ADR-0009) when
    /// non-empty. Delivered as a `--evidence-json` argument (structured,
    /// bounded data, unlike `summary_body`).
    pub evidence: &'a [EvidenceRow],
    /// The PR already opened for this run in an earlier attempt (issue #15
    /// review, H3): `Some` on a reboucle (this run has already been through
    /// this tail once), `None` only on a run's first pass. See
    /// `warden_gated::run_tail::RunTailRequest::existing_pr_number`'s docs
    /// for why this matters (a real PR provider rejects a second draft PR
    /// for the same branch).
    pub existing_pr_number: Option<u64>,
}

/// Requests `warden-gated` to (re)start a run's post-`Converged` tail
/// (ADR-0011: "`warden` possède le déclencheur du watch"). Both methods
/// return once the request has been successfully *issued*, not once the
/// tail has completed -- the eventual terminal outcome arrives later, over
/// `ci_result_socket`, delivered by `warden-gated` itself. The returned
/// [`GateChild`] lets the caller observe when the triggered subprocess exits,
/// so its wait on that outcome is bounded by the child's liveness (issue #15
/// review, M-new-1).
#[allow(async_fn_in_trait)]
pub trait GateTrigger {
    /// Starts the fresh tail: skeleton commit + `OpenDraft` + `Finalize` +
    /// `watch_pr`. Triggered once, on first entering `AwaitingCi`.
    async fn trigger_run_tail(&self, request: &RunTailTrigger<'_>) -> Result<GateChild>;

    /// Resumes watching an already-opened, already-finalized PR (Phase 6
    /// crash recovery, ADR-0011): `OpenDraft`/`Finalize` are not repeated.
    async fn trigger_resume_watch(
        &self,
        run_id: &str,
        pr_number: u64,
        ci_result_socket: &Path,
    ) -> Result<GateChild>;
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
        spawn_watched(command).await
    }
}

/// Spawns `command` with `stdin` piped and immediately written/closed, then
/// returns a [`GateChild`] tracking the child's exit (issue #15 review,
/// M-new-1) -- `warden` never awaits the child inline, but must be able to
/// observe when it goes away so a run whose gated subprocess dies without
/// delivering a terminal message is failed rather than waited on forever.
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

/// Reaps `child` in a detached task (logging a non-zero exit) and signals a
/// [`GateChild`] once it has exited -- covering both a clean exit and the
/// waiter task itself going away, so the orchestrator's `select!` can never
/// block forever on a child that is already gone.
fn watch_child_exit(mut child: tokio::process::Child, subcommand: &'static str) -> GateChild {
    let (exited_tx, exited_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) if !status.success() => {
                tracing::warn!(?status, subcommand, "warden-gated subprocess exited non-zero");
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, subcommand, "failed to wait on warden-gated subprocess");
            }
        }
        // A dropped receiver just means the orchestrator already moved on.
        let _ = exited_tx.send(());
    });
    // If the waiter task is ever cancelled, `exited_tx` drops and the awaited
    // `Err(RecvError)` still counts as "the child is gone" -- never a hang.
    GateChild::new(async move {
        let _ = exited_rx.await;
    })
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
