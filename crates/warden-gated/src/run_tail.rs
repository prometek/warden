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
    /// The PR already opened for this run in an earlier attempt (issue #15
    /// review, H3): a reboucled run (`ChecksFailed` -> `CoderRunning` -> ...
    /// -> `Converged` again) re-enters this tail with the *same* run, so
    /// `Some(pr_number)` here skips the skeleton commit and `OpenDraft`
    /// entirely and goes straight to `Finalize` against the existing PR --
    /// opening a second draft PR for the same branch would be rejected by a
    /// real PR provider (one open PR per branch) and silently orphan the
    /// first. `None` only for a run's first pass through this tail.
    pub existing_pr_number: Option<u64>,
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
    let pr = match request.existing_pr_number {
        // Issue #15 review, H3: a reboucle reuses the PR a prior pass
        // through this tail already opened -- never call `OpenDraft` again
        // for the same run.
        Some(pr_number) => PrHandle { number: pr_number },
        None => {
            let skeleton_commit_sha = create_skeleton_commit(
                request.bare_repo_path,
                request.base_branch,
                request.pushed_commit_sha,
            )
            .await
            .map_err(|error| (None, error))?;

            pr_manager::open_draft(
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
            .map_err(|error| (None, error))?
        }
    };

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

    /// Resolves `refname` (e.g. a branch name) to its commit sha within
    /// `dir` -- used to inspect what a push actually landed on `origin`,
    /// as opposed to `head_sha`'s own repo-local `HEAD`.
    fn head_sha_of_ref(dir: &Path, refname: &str) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", refname])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git rev-parse {refname} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
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
        /// How many `open_draft` calls this provider tolerates before
        /// rejecting, standing in for a real PR provider refusing a second
        /// draft PR for a branch that already has one open (issue #15
        /// review, H3). `usize::MAX` (via [`FakeProvider::new`]) means
        /// "never reject".
        open_draft_budget: usize,
        open_draft_calls: std::sync::atomic::AtomicUsize,
    }

    impl FakeProvider {
        fn new(next_pr_number: u64, ci_responses: Vec<PrStatus>) -> Self {
            Self::new_with_open_draft_budget(next_pr_number, ci_responses, usize::MAX)
        }

        fn new_with_open_draft_budget(
            next_pr_number: u64,
            ci_responses: Vec<PrStatus>,
            open_draft_budget: usize,
        ) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                next_pr_number,
                ci_responses: std::sync::Mutex::new(ci_responses.into()),
                open_draft_budget,
                open_draft_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl PrProvider for FakeProvider {
        async fn open_draft(
            &self,
            params: &crate::pr_manager::OpenDraftParams<'_>,
        ) -> Result<PrHandle> {
            let call_number = self
                .open_draft_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call_number >= self.open_draft_budget {
                // Mirrors the real `gh pr create` rejection for a branch
                // that already has an open PR.
                return Err(GatedError::GhCommandFailed {
                    command: "gh pr create".to_string(),
                    exit_code: Some(1),
                    stderr: format!(
                        "a pull request for branch \"{}\" into branch \"main\" already exists",
                        params.branch
                    ),
                });
            }
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

        async fn update_body(&self, pr: &PrHandle, body: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("update_body({}, {body})", pr.number));
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
            existing_pr_number: None,
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
            existing_pr_number: None,
        };

        let message = run_tail(&pool, &request, &provider).await;

        assert_eq!(message.pr_number, Some(7));
        assert!(matches!(message.outcome, CiWatchOutcome::GateFailed { .. }));
    }

    /// Issue #15 review, H3: a reboucled run (`ChecksFailed` -> `CoderRunning`
    /// -> ... -> `Converged` again) re-enters `run_tail` a second time. With
    /// a provider whose `open_draft` REJECTS any call past the first (mirrors
    /// a real PR provider refusing a duplicate draft PR for the same
    /// branch), the second pass must still succeed by reusing
    /// `existing_pr_number` -- never calling `open_draft` again.
    #[tokio::test]
    async fn run_tail_reboucle_reuses_the_existing_pr_and_never_calls_open_draft_again() {
        let (_origin, gate_repo, business_sha) = converged_gate_repo_fixture();
        let (_db_dir, pool) = seeded_db("run-1", &business_sha).await;
        let provider = FakeProvider::new_with_open_draft_budget(
            42,
            vec![
                PrStatus {
                    lifecycle: PrLifecycle::Open,
                    checks: vec![CheckRun {
                        name: "build".to_string(),
                        conclusion: CheckConclusion::Failed,
                        details_url: None,
                    }],
                },
                PrStatus {
                    lifecycle: PrLifecycle::Open,
                    checks: vec![CheckRun {
                        name: "build".to_string(),
                        conclusion: CheckConclusion::Passed,
                        details_url: None,
                    }],
                },
            ],
            1,
        );

        // First pass: no PR yet, so `open_draft` is called (and allowed,
        // this is call #1 within the budget of 1).
        let first_request = RunTailRequest {
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
            existing_pr_number: None,
        };
        let first_message = run_tail(&pool, &first_request, &provider).await;
        assert_eq!(first_message.pr_number, Some(42));
        assert!(matches!(
            first_message.outcome,
            CiWatchOutcome::ChecksFailed { .. }
        ));

        // Second pass (the reboucle): the run converged again on the same
        // commit (finalize/push are idempotent, see pr_manager::finalize's
        // own docs) -- what matters here is that `existing_pr_number` is
        // now `Some`, so this must NOT call `open_draft` again (the fake
        // provider would reject a second call).
        let second_request = RunTailRequest {
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
            existing_pr_number: Some(42),
        };
        let second_message = run_tail(&pool, &second_request, &provider).await;

        assert_eq!(second_message.pr_number, Some(42));
        assert_eq!(second_message.outcome, CiWatchOutcome::checks_passed());
        assert_eq!(
            provider
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.starts_with("open_draft"))
                .count(),
            1,
            "open_draft must only ever be called once across both passes"
        );
    }

    /// Adds a second, later commit on top of `gate_repo`'s existing
    /// "business" commit (its local `main`) -- a fast-forward descendant,
    /// simulating the coder producing genuinely new content on a reboucle's
    /// next cycle. Returns the new commit's sha.
    fn add_second_business_commit(gate_repo: &TempDir) -> String {
        let work = TempDir::new().unwrap();
        run_git(
            work.path(),
            &[
                "clone",
                "--quiet",
                &gate_repo.path().display().to_string(),
                ".",
            ],
        );
        run_git(work.path(), &["config", "user.email", "test@warden.local"]);
        run_git(work.path(), &["config", "user.name", "warden-test"]);
        std::fs::write(work.path().join("fix.txt"), "fixed the build\n").unwrap();
        run_git(work.path(), &["add", "."]);
        run_git(work.path(), &["commit", "--quiet", "-m", "fix the build"]);
        let sha = head_sha(work.path());
        run_git(work.path(), &["push", "--quiet", "origin", "HEAD:main"]);
        sha
    }

    /// Issue #15 review, H3: a reboucle must push the *new* cycle's content
    /// to the existing PR, never silently re-finalize the previous (by now
    /// stale) commit or PR body. Runs two genuinely different commits (and
    /// two different summary bodies) through `run_tail`, reusing the same
    /// PR via `existing_pr_number`, and asserts both that `origin`'s branch
    /// actually moved to the second commit and that the PR body delivered
    /// on the second pass carries the second pass's own text -- not the
    /// first's.
    #[tokio::test]
    async fn run_tail_reboucle_pushes_the_new_cycles_content_not_the_stale_one() {
        let (origin, gate_repo, first_commit_sha) = converged_gate_repo_fixture();
        let second_commit_sha = add_second_business_commit(&gate_repo);
        assert_ne!(
            first_commit_sha, second_commit_sha,
            "the fixture must produce two genuinely different commits"
        );
        let (_db_dir, pool) = seeded_db("run-1", &first_commit_sha).await;
        let provider = FakeProvider::new_with_open_draft_budget(
            42,
            vec![
                PrStatus {
                    lifecycle: PrLifecycle::Open,
                    checks: vec![CheckRun {
                        name: "build".to_string(),
                        conclusion: CheckConclusion::Failed,
                        details_url: None,
                    }],
                },
                PrStatus {
                    lifecycle: PrLifecycle::Open,
                    checks: vec![CheckRun {
                        name: "build".to_string(),
                        conclusion: CheckConclusion::Passed,
                        details_url: None,
                    }],
                },
            ],
            1,
        );

        // First pass: opens the PR, pushes the first commit under the
        // first pass's own summary.
        let first_request = RunTailRequest {
            bare_repo_path: gate_repo.path(),
            run_id: "run-1",
            intent: "Add a feature",
            branch: "warden/run-1",
            base_branch: "main",
            pushed_commit_sha: &first_commit_sha,
            summary_body: "Cycle 1 summary",
            evidence: &[],
            repo_slug: "owner/repo",
            watch_config: watch_config(),
            existing_pr_number: None,
        };
        let first_message = run_tail(&pool, &first_request, &provider).await;
        assert_eq!(first_message.pr_number, Some(42));
        assert!(matches!(
            first_message.outcome,
            CiWatchOutcome::ChecksFailed { .. }
        ));

        // Between passes: the run reboucled and re-converged on genuinely
        // new content -- persisted state now points at the second commit,
        // exactly what `warden`'s own `drive_post_convergence_tail` would
        // have written before re-triggering this tail.
        sqlx::query("UPDATE runs SET converged_commit_sha = ? WHERE id = ?")
            .bind(&second_commit_sha)
            .bind("run-1")
            .execute(&pool)
            .await
            .unwrap();

        let second_request = RunTailRequest {
            bare_repo_path: gate_repo.path(),
            run_id: "run-1",
            intent: "Add a feature",
            branch: "warden/run-1",
            base_branch: "main",
            pushed_commit_sha: &second_commit_sha,
            summary_body: "Cycle 2 summary (fixed the build)",
            evidence: &[],
            repo_slug: "owner/repo",
            watch_config: watch_config(),
            existing_pr_number: Some(42),
        };
        let second_message = run_tail(&pool, &second_request, &provider).await;

        assert_eq!(second_message.pr_number, Some(42));
        assert_eq!(second_message.outcome, CiWatchOutcome::checks_passed());

        let origin_branch_head = head_sha_of_ref(origin.path(), "warden/run-1");
        assert_eq!(
            origin_branch_head, second_commit_sha,
            "the reboucle must push the new cycle's commit to the existing PR's branch on \
             origin, not leave it on the stale first-cycle commit"
        );

        let calls = provider.calls.lock().unwrap();
        let last_update_body = calls
            .iter()
            .rev()
            .find(|c| c.starts_with("update_body("))
            .expect("update_body must have been called on the second pass");
        assert!(
            last_update_body.starts_with("update_body(42, Cycle 2 summary"),
            "the reboucle's own (most recent) update_body call must carry the new cycle's own \
             content, not be missing or stale: {calls:?}"
        );
        assert!(
            !last_update_body.contains("Cycle 1 summary"),
            "the reboucle's own (most recent) update_body call must not still carry the stale \
             first-cycle PR body text: {calls:?}"
        );
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
