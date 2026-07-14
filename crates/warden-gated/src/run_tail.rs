//! Composes the post-`Converged` tail issue #15 wires up: a content-free
//! skeleton commit, `pr_manager::open_draft`, `pr_manager::finalize`, and
//! `ci_watcher::watch_pr`, folding whatever succeeds or fails along the way
//! into the one terminal [`CiResultMessage`] `warden` is waiting for
//! (ADR-0011). Also [`resume_watch`], the crash-recovery counterpart that
//! re-derives a PR already opened/finalized in an earlier attempt and just
//! resumes watching it -- `warden-gated` keeps no watch state of its own.
//!
//! This module never propagates a bare `Result` to its callers: its whole
//! job is to always produce a terminal message, converting any internal
//! failure into [`warden_core::CiWatchOutcome::GateFailed`] rather than
//! letting the caller (a CLI subcommand) decide what "no message at all"
//! should mean.

use std::path::Path;

use sqlx::SqlitePool;
use tokio::process::Command;
use warden_core::{CiResultMessage, CiWatchOutcome, EvidenceRow};

use crate::ci_watcher::{watch_pr, CiProvider, WatchConfig, WatchOutcome};
use crate::error::{GatedError, Result};
use crate::pr_manager::{
    self, fetch_branch, remote_branch_head, FinalizeOutcome, FinalizeRequest, OpenDraftRequest,
    PrHandle, PrProvider, EMPTY_TREE_SHA,
};

/// Everything the fresh (first-time) tail needs beyond the read-only pool
/// and provider: the run's identity/intent and the commit already pushed
/// into the bare gate repo (mirrors `FinalizeRequest`'s own fields, plus
/// what `OpenDraft` additionally needs).
pub struct RunTailRequest<'a> {
    pub bare_repo_path: &'a Path,
    pub run_id: &'a str,
    pub intent: &'a str,
    /// The run's own branch (e.g. `warden/<run_id>`), pushed under both the
    /// skeleton and the final content.
    pub branch: &'a str,
    pub base_branch: &'a str,
    /// The commit already pushed into the bare gate repo -- checked against
    /// `runs.converged_commit_sha` by `pr_manager::finalize`'s own
    /// re-verification, never trusted as-is.
    pub pushed_commit_sha: &'a str,
    pub summary_body: &'a str,
    pub evidence: &'a [EvidenceRow],
    pub repo_slug: &'a str,
    pub watch_config: WatchConfig,
}

/// Runs the full fresh tail: skeleton commit -> `OpenDraft` -> `Finalize` ->
/// `watch_pr`, returning the one terminal message `warden` is waiting for.
/// Every fallible step converts its error into
/// [`CiWatchOutcome::GateFailed`] rather than propagating -- see module docs.
pub async fn run_tail<P: PrProvider + CiProvider>(
    pool: &SqlitePool,
    request: &RunTailRequest<'_>,
    provider: &P,
) -> CiResultMessage {
    match run_tail_fallible(pool, request, provider).await {
        Ok(message) => message,
        Err((pr_number, error)) => CiResultMessage {
            run_id: request.run_id.to_string(),
            pr_number,
            outcome: CiWatchOutcome::gate_failed(error.to_string()),
        },
    }
}

/// The fallible core of [`run_tail`]. Returns `Err((pr_number, error))` so a
/// failure past `OpenDraft` can still report the PR number it managed to
/// open, even though the tail didn't reach a watchable state.
async fn run_tail_fallible<P: PrProvider + CiProvider>(
    pool: &SqlitePool,
    request: &RunTailRequest<'_>,
    provider: &P,
) -> std::result::Result<CiResultMessage, (Option<u64>, GatedError)> {
    let skeleton_commit_sha = create_skeleton_commit(
        request.bare_repo_path,
        request.base_branch,
        request.pushed_commit_sha,
    )
    .await
    .map_err(|error| (None, error))?;

    let pr = pr_manager::open_draft(
        &OpenDraftRequest {
            bare_repo_path: request.bare_repo_path,
            skeleton_commit_sha: &skeleton_commit_sha,
            branch: request.branch,
            base_branch: request.base_branch,
            intent: request.intent,
        },
        provider,
    )
    .await
    .map_err(|error| (None, error))?;

    let finalize_outcome = pr_manager::finalize(
        pool,
        &FinalizeRequest {
            bare_repo_path: request.bare_repo_path,
            branch: request.branch,
            run_id: request.run_id,
            pushed_commit_sha: request.pushed_commit_sha,
            pr: &pr,
            summary_body: request.summary_body,
            evidence: request.evidence,
            repo_slug: request.repo_slug,
        },
        provider,
    )
    .await
    .map_err(|error| (Some(pr.number), error))?;

    match finalize_outcome {
        FinalizeOutcome::Blocked(reason) => Err((
            Some(pr.number),
            GatedError::FinalizeBlocked {
                reason: format!("{reason:?}"),
            },
        )),
        FinalizeOutcome::Finalized { .. } => {
            let outcome = watch_pr(&pr, provider, &request.watch_config)
                .await
                .map_err(|error| (Some(pr.number), error))?;
            Ok(CiResultMessage {
                run_id: request.run_id.to_string(),
                pr_number: Some(pr.number),
                outcome: watch_outcome_to_wire(outcome),
            })
        }
    }
}

/// The crash-recovery counterpart of [`run_tail`]: `OpenDraft`/`Finalize`
/// already happened in an earlier attempt (`pr_number` is read back from
/// `warden`'s own `runs.pr_number`, ADR-0011), so this only resumes
/// `watch_pr` -- `warden-gated` keeps no watch state of its own; GitHub is
/// the durable source of truth being re-polled from scratch.
pub async fn resume_watch<P: CiProvider>(
    run_id: &str,
    pr_number: u64,
    provider: &P,
    watch_config: &WatchConfig,
) -> CiResultMessage {
    let pr = PrHandle { number: pr_number };
    match watch_pr(&pr, provider, watch_config).await {
        Ok(outcome) => CiResultMessage {
            run_id: run_id.to_string(),
            pr_number: Some(pr_number),
            outcome: watch_outcome_to_wire(outcome),
        },
        Err(error) => CiResultMessage {
            run_id: run_id.to_string(),
            pr_number: Some(pr_number),
            outcome: CiWatchOutcome::gate_failed(error.to_string()),
        },
    }
}

/// Maps `ci_watcher::WatchOutcome` (which carries real `Finding`s, for
/// human-readable reporting) onto the wire-serializable `CiWatchOutcome`
/// (`warden-core` cannot depend on `warden-gated`, ADR-0006, so this
/// conversion has to live on this side of the boundary).
fn watch_outcome_to_wire(outcome: WatchOutcome) -> CiWatchOutcome {
    match outcome {
        WatchOutcome::Merged => CiWatchOutcome::merged(),
        WatchOutcome::Closed => CiWatchOutcome::closed(),
        WatchOutcome::ChecksPassed => CiWatchOutcome::checks_passed(),
        WatchOutcome::ChecksFailed(findings) => CiWatchOutcome::checks_failed(&findings),
        WatchOutcome::TimedOut => CiWatchOutcome::timed_out(),
    }
}

/// Finds a commit to use as `OpenDraft`'s skeleton: the merge-base of
/// `pushed_commit_sha` (the run's own converged commit, already in
/// `bare_repo_path`'s object store) and `base_branch`'s current tip on
/// `origin`. Two properties fall out of that choice, both required for this
/// module's simplified issue-#15 sequencing (`OpenDraft` immediately
/// followed by `Finalize`, rather than `OpenDraft` at coder-start per
/// ADR-0007's original modeling):
///
/// 1. **Content-free relative to `base_branch`**, as long as `base_branch`
///    hasn't moved since this run diverged from it -- the merge-base *is*
///    the commit the run branched from, so its tree matches `base_branch`'s
///    tip exactly in the common case. If `base_branch` moved in the
///    meantime, `open_draft`'s own independent re-verification
///    (`skeleton_diff_against_base`) still catches and refuses a non-empty
///    diff -- this function does not weaken that check, it only tries to
///    pick a skeleton that passes it in the ordinary case.
/// 2. **An ancestor of `pushed_commit_sha`**, so `finalize`'s later push of
///    the real content onto the same branch is always a fast-forward, never
///    rejected by `origin` the way pushing two unrelated histories onto the
///    same ref would be.
///
/// Falls back to the well-known empty-tree commit when `base_branch`
/// doesn't exist on `origin` yet (first-ever run against a fresh repo) --
/// only genuinely content-free if `pushed_commit_sha`'s own root commit is
/// itself empty, a known limitation of this simplified path, acceptable
/// since real usage always pushes an initial commit to `base_branch` first.
async fn create_skeleton_commit(
    bare_repo_path: &Path,
    base_branch: &str,
    pushed_commit_sha: &str,
) -> Result<String> {
    let base_tip = remote_branch_head(bare_repo_path, "origin", base_branch).await?;
    let Some(base_tip) = base_tip else {
        return Ok(EMPTY_TREE_SHA.to_string());
    };
    fetch_branch(bare_repo_path, "origin", base_branch).await?;

    git_stdout(
        bare_repo_path,
        &["merge-base", pushed_commit_sha, &base_tip],
    )
    .await
}

/// Runs `git <args>` in `repo_path` and returns its trimmed stdout, failing
/// loudly (command, exit status, stderr attached) on a non-zero exit.
async fn git_stdout(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        return Err(GatedError::GitCommandFailed {
            command: format!("git -C {} {}", repo_path.display(), args.join(" ")),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci_watcher::{CheckConclusion, CheckRun, PrLifecycle, PrStatus};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::time::Duration;
    use tempfile::TempDir;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn head_sha(dir: &Path) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A bare gate repo whose local `main` sits one "business" commit ahead
    /// of `origin/main` -- the shape a real converged run leaves behind:
    /// `origin` only knows the pre-run baseline, while the bare gate repo
    /// (which `warden` already pushed the converged commit into) has both.
    /// Returns `(origin, gate_repo, business_commit_sha)`.
    fn converged_gate_repo_fixture() -> (TempDir, TempDir, String) {
        let origin = TempDir::new().unwrap();
        run_git(origin.path(), &["init", "--bare", "--quiet"]);

        let seed = TempDir::new().unwrap();
        run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
        run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
        run_git(seed.path(), &["config", "user.name", "warden-test"]);
        std::fs::write(seed.path().join("f.txt"), "base\n").unwrap();
        run_git(seed.path(), &["add", "."]);
        run_git(seed.path(), &["commit", "--quiet", "-m", "base"]);
        run_git(
            seed.path(),
            &[
                "push",
                "--quiet",
                &origin.path().display().to_string(),
                "main",
            ],
        );

        std::fs::write(seed.path().join("feature.txt"), "business logic\n").unwrap();
        run_git(seed.path(), &["add", "."]);
        run_git(seed.path(), &["commit", "--quiet", "-m", "add feature"]);
        let business_commit_sha = head_sha(seed.path());

        let gate_repo = TempDir::new().unwrap();
        run_git(
            gate_repo.path(),
            &[
                "clone",
                "--bare",
                "--quiet",
                &seed.path().display().to_string(),
                ".",
            ],
        );
        run_git(
            gate_repo.path(),
            &[
                "remote",
                "set-url",
                "origin",
                &origin.path().display().to_string(),
            ],
        );

        (origin, gate_repo, business_commit_sha)
    }

    /// A real, temporary SQLite database seeded with one `runs` row --
    /// mirrors `verify.rs`/`serve.rs`'s own test fixtures rather than a mock
    /// (code-standards.md: "DB de test: SQLite fichier temporaire réel").
    async fn seeded_db(run_id: &str, converged_commit_sha: &str) -> (TempDir, SqlitePool) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE runs (id TEXT PRIMARY KEY, state TEXT NOT NULL, converged_commit_sha TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO runs (id, state, converged_commit_sha) VALUES (?, 'converged', ?)",
        )
        .bind(run_id)
        .bind(converged_commit_sha)
        .execute(&pool)
        .await
        .unwrap();

        (dir, pool)
    }

    /// Fakes both `PrProvider` and `CiProvider` without any real `gh`/
    /// network call (code-standards.md: "pas d'appel réseau externe") --
    /// mirrors `pr_manager::tests::RecordingProvider` and
    /// `ci_watcher::tests::ScriptedProvider`, combined into one type since
    /// `run_tail` needs both bounds on the same provider.
    struct FakeProvider {
        calls: std::sync::Mutex<Vec<String>>,
        next_pr_number: u64,
        ci_responses: std::sync::Mutex<std::collections::VecDeque<PrStatus>>,
    }

    impl FakeProvider {
        fn new(next_pr_number: u64, ci_responses: Vec<PrStatus>) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                next_pr_number,
                ci_responses: std::sync::Mutex::new(ci_responses.into()),
            }
        }
    }

    impl PrProvider for FakeProvider {
        async fn open_draft(
            &self,
            params: &crate::pr_manager::OpenDraftParams<'_>,
        ) -> Result<PrHandle> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("open_draft({})", params.branch));
            Ok(PrHandle {
                number: self.next_pr_number,
            })
        }

        async fn post_comment(&self, pr: &PrHandle, _body: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("post_comment({})", pr.number));
            Ok(())
        }

        async fn mark_ready(&self, pr: &PrHandle) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("mark_ready({})", pr.number));
            Ok(())
        }

        async fn update_body(&self, pr: &PrHandle, _body: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("update_body({})", pr.number));
            Ok(())
        }
    }

    impl CiProvider for FakeProvider {
        async fn pr_status(&self, _pr: &PrHandle) -> Result<PrStatus> {
            Ok(self
                .ci_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("FakeProvider ran out of scripted CI responses"))
        }
    }

    fn watch_config() -> WatchConfig {
        WatchConfig {
            poll_interval: Duration::from_millis(1),
            inactivity_timeout: Duration::from_secs(3600),
            max_consecutive_poll_errors: WatchConfig::DEFAULT_MAX_CONSECUTIVE_POLL_ERRORS,
        }
    }

    #[tokio::test]
    async fn run_tail_opens_finalizes_and_reports_checks_passed() {
        let (_origin, gate_repo, business_sha) = converged_gate_repo_fixture();
        let (_db_dir, pool) = seeded_db("run-1", &business_sha).await;
        let provider = FakeProvider::new(
            42,
            vec![PrStatus {
                lifecycle: PrLifecycle::Open,
                checks: vec![CheckRun {
                    name: "build".to_string(),
                    conclusion: CheckConclusion::Passed,
                    details_url: None,
                }],
            }],
        );

        let request = RunTailRequest {
            bare_repo_path: gate_repo.path(),
            run_id: "run-1",
            intent: "Add a feature",
            branch: "warden/run-1",
            base_branch: "main",
            pushed_commit_sha: &business_sha,
            summary_body: "Cycle summary",
            evidence: &[],
            repo_slug: "owner/repo",
            watch_config: watch_config(),
        };

        let message = run_tail(&pool, &request, &provider).await;

        assert_eq!(message.run_id, "run-1");
        assert_eq!(message.pr_number, Some(42));
        assert_eq!(message.outcome, CiWatchOutcome::checks_passed());
        assert!(provider
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("open_draft")));
        assert!(provider
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("mark_ready")));
    }

    /// A run whose real, persisted `converged_commit_sha` no longer matches
    /// `pushed_commit_sha` (state drifted between push and tail) must
    /// surface as `GateFailed`, carrying the PR number `OpenDraft` still
    /// managed to open -- never silently dropped, never crashing the tail.
    #[tokio::test]
    async fn run_tail_reports_gate_failed_when_finalize_is_blocked() {
        let (_origin, gate_repo, business_sha) = converged_gate_repo_fixture();
        let (_db_dir, pool) = seeded_db("run-1", "a-different-sha-than-what-was-pushed").await;
        let provider = FakeProvider::new(7, vec![]);

        let request = RunTailRequest {
            bare_repo_path: gate_repo.path(),
            run_id: "run-1",
            intent: "Add a feature",
            branch: "warden/run-1",
            base_branch: "main",
            pushed_commit_sha: &business_sha,
            summary_body: "Cycle summary",
            evidence: &[],
            repo_slug: "owner/repo",
            watch_config: watch_config(),
        };

        let message = run_tail(&pool, &request, &provider).await;

        assert_eq!(message.pr_number, Some(7));
        assert!(matches!(message.outcome, CiWatchOutcome::GateFailed { .. }));
    }

    #[tokio::test]
    async fn resume_watch_reports_the_terminal_outcome_without_touching_pr_manager() {
        let provider = FakeProvider::new(
            9,
            vec![PrStatus {
                lifecycle: PrLifecycle::Merged,
                checks: vec![],
            }],
        );

        let message = resume_watch("run-2", 99, &provider, &watch_config()).await;

        assert_eq!(message.run_id, "run-2");
        assert_eq!(message.pr_number, Some(99));
        assert_eq!(message.outcome, CiWatchOutcome::merged());
        assert!(
            provider.calls.lock().unwrap().is_empty(),
            "resume_watch must never call any PrProvider method"
        );
    }
}
