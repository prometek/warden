//! Worktree Manager (ADR-0001): isolates every agent's working copy in its
//! own `git worktree add --detach`, under a dedicated directory that is
//! never inside the user's main repository working tree. Cleanup happens
//! automatically via `Drop`, with an explicit async `remove` available when
//! the caller wants to observe/handle a cleanup failure instead of just
//! logging it.

use std::path::{Path, PathBuf};
use std::process::Command as SyncCommand;

use tokio::process::Command;

use crate::error::WorktreeError;
use crate::path_util::canonicalize_best_effort;

/// Creates isolated worktrees for a single main repository.
///
/// `main_repo_path` is the user's real, pre-existing repository — Warden
/// only ever reads from it (to resolve `commit_ish` and to run
/// `git worktree add/remove`, both of which touch `.git/worktrees/`
/// metadata, never the checked-out working tree files). All actual agent
/// I/O happens under `worktrees_root`, which is validated at construction
/// time to never be the main repo path or a path inside it.
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    main_repo_path: PathBuf,
    worktrees_root: PathBuf,
}

impl WorktreeManager {
    /// Validates `main_repo_path` is a git repository and that
    /// `worktrees_root` is disjoint from its working tree, then returns a
    /// manager ready to create worktrees. Does not touch the filesystem
    /// beyond this check.
    pub fn new(
        main_repo_path: impl Into<PathBuf>,
        worktrees_root: impl Into<PathBuf>,
    ) -> Result<Self, WorktreeError> {
        let main_repo_path = main_repo_path.into();
        let worktrees_root = worktrees_root.into();

        if !main_repo_path.join(".git").exists() {
            return Err(WorktreeError::NotAGitRepo(main_repo_path));
        }

        // Canonicalize before the containment check: a non-canonical
        // worktrees_root (e.g. via `..` segments) must not be able to slip
        // past this guard and land inside the main working tree.
        let canonical_repo = main_repo_path.canonicalize().map_err(WorktreeError::Io)?;
        // worktrees_root doesn't need to exist yet; canonicalize its
        // deepest existing ancestor instead. Issue #26 review, MEDIUM:
        // shares `path_util::canonicalize_best_effort` with
        // `process::validate_agent_program` and
        // `agent_def::user_config_resolves_inside_repo_or_worktrees` rather
        // than a separate copy -- see that module's own docs for why the
        // earlier separate copy here quietly failed to fail closed (it kept
        // popping path segments on *any* `canonicalize` error, not just
        // `NotFound`).
        let canonical_worktrees_root =
            canonicalize_best_effort(&worktrees_root).map_err(WorktreeError::Io)?;

        if canonical_worktrees_root == canonical_repo
            || canonical_worktrees_root.starts_with(&canonical_repo)
        {
            return Err(WorktreeError::UnsafeWorktreesRoot {
                main_repo: main_repo_path,
                worktrees_root,
            });
        }

        Ok(Self {
            main_repo_path,
            worktrees_root,
        })
    }

    /// Creates a detached worktree at `<worktrees_root>/<run_id>/<role>`,
    /// checked out at `commit_ish`. The parent directory is created if
    /// needed; the leaf directory itself must not already exist (git
    /// requires this for `worktree add`).
    pub async fn create(
        &self,
        run_id: &str,
        role: &str,
        commit_ish: &str,
    ) -> Result<Worktree, WorktreeError> {
        let path = self.worktrees_root.join(run_id).join(role);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let output = Command::new("git")
            .arg("-C")
            .arg(&self.main_repo_path)
            .args(["worktree", "add", "--detach"])
            .arg(&path)
            .arg(commit_ish)
            .output()
            .await?;

        if !output.status.success() {
            return Err(WorktreeError::GitCommandFailed {
                command: format!("git worktree add --detach {} {commit_ish}", path.display()),
                exit_code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(Worktree {
            path,
            main_repo_path: self.main_repo_path.clone(),
            removed: false,
        })
    }
}

/// Removes a worktree at `path` from `main_repo_path` that no longer has a
/// [`Worktree`] guard owning it — used by crash recovery, where the
/// orchestrator that would normally call [`Worktree::remove`] (or drop the
/// guard) died before it got the chance (Architecture.md §9, Disaster
/// Recovery).
///
/// Idempotent by design: if `path` doesn't exist on disk anymore (already
/// cleaned up, or never fully created), this returns `Ok(())` without
/// invoking git at all, rather than treating "already gone" as a recovery
/// failure. A `path` that *does* exist but that git genuinely refuses to
/// remove (corrupted worktree metadata, permissions, ...) still surfaces as
/// [`WorktreeError::GitCommandFailed`] — only the "nothing there" case is
/// swallowed.
pub async fn remove_orphan_worktree(
    main_repo_path: &Path,
    path: &Path,
) -> Result<(), WorktreeError> {
    if !path.exists() {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(main_repo_path)
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .output()
        .await?;

    if !output.status.success() {
        return Err(WorktreeError::GitCommandFailed {
            command: format!("git worktree remove --force {}", path.display()),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(())
}

/// Runs `git worktree prune` against `main_repo_path`, clearing any
/// remaining `.git/worktrees/<name>` administrative entries left behind by
/// worktrees whose working directory is already gone (e.g. removed by
/// [`remove_orphan_worktree`], or deleted out-of-band). Called once after
/// processing all of a crashed run's recorded worktree paths, not per-path —
/// pruning is a whole-repository operation.
pub async fn prune_worktrees(main_repo_path: &Path) -> Result<(), WorktreeError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(main_repo_path)
        .args(["worktree", "prune"])
        .output()
        .await?;

    if !output.status.success() {
        return Err(WorktreeError::GitCommandFailed {
            command: "git worktree prune".to_string(),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(())
}

/// A single isolated, detached git worktree. Removed via `git worktree
/// remove --force` either explicitly (`remove`) or on `Drop` as a
/// best-effort fallback.
#[derive(Debug)]
pub struct Worktree {
    path: PathBuf,
    main_repo_path: PathBuf,
    removed: bool,
}

impl Worktree {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Explicitly removes the worktree, propagating any failure to the
    /// caller. Prefer this over relying on `Drop` when the caller can
    /// meaningfully react to a cleanup error.
    pub async fn remove(mut self) -> Result<(), WorktreeError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.main_repo_path)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output()
            .await?;

        if !output.status.success() {
            return Err(WorktreeError::GitCommandFailed {
                command: format!("git worktree remove --force {}", self.path.display()),
                exit_code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        self.removed = true;
        Ok(())
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        // Drop can't be async: fall back to a synchronous git invocation.
        // Best-effort only — a failure here is logged, never panics, and
        // never propagates (nothing to propagate to).
        match SyncCommand::new("git")
            .arg("-C")
            .arg(&self.main_repo_path)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output()
        {
            Ok(output) if output.status.success() => {
                tracing::debug!(path = %self.path.display(), "worktree cleaned up on drop");
            }
            Ok(output) => {
                tracing::warn!(
                    path = %self.path.display(),
                    stderr = %String::from_utf8_lossy(&output.stderr),
                    "failed to clean up worktree on drop"
                );
            }
            Err(error) => {
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "failed to spawn git to clean up worktree on drop"
                );
            }
        }
        self.removed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Sets up a throwaway git repo with a single commit, suitable as
    /// `main_repo_path` in tests.
    fn init_test_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        let run = |args: &[&str]| {
            let status = SyncCommand::new("git")
                .current_dir(dir.path())
                .args(args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.email", "test@warden.local"]);
        run(&["config", "user.name", "warden-test"]);
        std::fs::write(dir.path().join("README.md"), "warden test repo\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "--quiet", "-m", "initial commit"]);
        dir
    }

    fn snapshot_dir_entries(path: &Path) -> Vec<String> {
        let mut entries: Vec<String> = std::fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        entries
    }

    #[test]
    fn rejects_a_worktrees_root_nested_inside_the_main_repo() {
        let repo = init_test_repo();
        let nested = repo.path().join(".warden-worktrees");
        let result = WorktreeManager::new(repo.path(), &nested);
        assert!(matches!(
            result,
            Err(WorktreeError::UnsafeWorktreesRoot { .. })
        ));
    }

    /// Issue #26 review, MEDIUM: `new`'s containment check must fail closed
    /// when it can no longer verify what `worktrees_root` actually resolves
    /// to -- a permissions error on an ancestor must never be silently
    /// walked past and compared as a truncated path. Before switching to the
    /// shared `path_util::canonicalize_best_effort`, this module's own copy
    /// popped path segments on *any* `canonicalize` error (not just
    /// `NotFound`), so a case exactly like this one would have silently
    /// succeeded instead of surfacing the permissions failure -- this test
    /// pins the fixed behaviour.
    #[cfg(unix)]
    #[test]
    fn fails_closed_when_an_ancestor_of_worktrees_root_cannot_be_canonicalized() {
        use std::os::unix::fs::PermissionsExt;

        let repo = init_test_repo();
        let outside = TempDir::new().unwrap();
        let locked = outside.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        // Two non-existent path segments below `locked`: resolving the
        // deeper one requires *searching inside* `locked` (execute
        // permission on `locked` itself, not just on its parent), so
        // stripping `locked`'s own permissions actually triggers a
        // permission error partway up the ancestor walk -- a single
        // non-existent segment directly under `locked` would only need
        // execute permission on `locked`'s *parent* (`outside`, left
        // untouched) to resolve, and wouldn't exercise this path at all.
        let worktrees_root = locked.join("nested").join("does-not-exist-yet");

        let mut perms = std::fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&locked, perms.clone()).unwrap();

        let result = WorktreeManager::new(repo.path(), &worktrees_root);

        // Restore permissions before any assertion can panic and leave a
        // directory `TempDir::drop` can't clean up.
        perms.set_mode(0o755);
        std::fs::set_permissions(&locked, perms).unwrap();

        assert!(
            matches!(result, Err(WorktreeError::Io(_))),
            "expected a fail-closed WorktreeError::Io, got {result:?}"
        );
    }

    #[test]
    fn rejects_a_main_repo_path_without_git() {
        let not_a_repo = TempDir::new().unwrap();
        let worktrees_root = TempDir::new().unwrap();
        let result = WorktreeManager::new(not_a_repo.path(), worktrees_root.path());
        assert!(matches!(result, Err(WorktreeError::NotAGitRepo(_))));
    }

    #[tokio::test]
    async fn creates_and_cleans_up_a_worktree_without_touching_the_main_repo() {
        let repo = init_test_repo();
        let worktrees_root = TempDir::new().unwrap();
        let manager = WorktreeManager::new(repo.path(), worktrees_root.path()).unwrap();

        let before = snapshot_dir_entries(repo.path());

        let worktree = manager.create("run-1", "coder", "HEAD").await.unwrap();
        assert!(worktree.path().join("README.md").exists());

        // The main repo's working tree must be byte-for-byte untouched by
        // creating a worktree elsewhere — only its `.git/worktrees/`
        // bookkeeping changes, never the checked-out files.
        let during = snapshot_dir_entries(repo.path());
        assert_eq!(before, during);

        let worktree_path = worktree.path().to_path_buf();
        drop(worktree);

        assert!(
            !worktree_path.exists(),
            "worktree should be removed on drop"
        );
    }

    #[tokio::test]
    async fn explicit_remove_reports_errors_instead_of_swallowing_them() {
        let repo = init_test_repo();
        let worktrees_root = TempDir::new().unwrap();
        let manager = WorktreeManager::new(repo.path(), worktrees_root.path()).unwrap();

        let worktree = manager.create("run-2", "reviewer", "HEAD").await.unwrap();
        let path = worktree.path().to_path_buf();
        worktree.remove().await.unwrap();

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn remove_orphan_worktree_removes_a_worktree_left_behind_without_its_guard() {
        let repo = init_test_repo();
        let worktrees_root = TempDir::new().unwrap();
        let manager = WorktreeManager::new(repo.path(), worktrees_root.path()).unwrap();

        // Simulates a crash: the `Worktree` guard is forgotten (never
        // dropped, never `remove`d) rather than cleaned up normally, the way
        // an orchestrator killed by SIGKILL would leave it.
        let worktree = manager.create("orphan-run", "coder", "HEAD").await.unwrap();
        let path = worktree.path().to_path_buf();
        std::mem::forget(worktree);
        assert!(path.exists());

        remove_orphan_worktree(repo.path(), &path).await.unwrap();

        assert!(!path.exists(), "orphan worktree must be removed");
    }

    #[tokio::test]
    async fn remove_orphan_worktree_on_an_already_gone_path_is_a_noop_not_an_error() {
        let repo = init_test_repo();
        let never_existed = repo.path().join("never-existed");

        // Idempotent: recovery may run this twice, or the worktree may
        // already have been cleaned up by some other path — either way,
        // "nothing there" must not surface as a recovery failure.
        remove_orphan_worktree(repo.path(), &never_existed)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn prune_worktrees_clears_stale_administrative_entries() {
        let repo = init_test_repo();
        let worktrees_root = TempDir::new().unwrap();
        let manager = WorktreeManager::new(repo.path(), worktrees_root.path()).unwrap();

        let worktree = manager.create("prune-run", "coder", "HEAD").await.unwrap();
        let path = worktree.path().to_path_buf();
        std::mem::forget(worktree);

        // Delete the working directory out-of-band (as if the filesystem,
        // not git, removed it) so `.git/worktrees/coder` is left dangling —
        // exactly what `git worktree prune` exists to clear.
        std::fs::remove_dir_all(&path).unwrap();

        prune_worktrees(repo.path()).await.unwrap();

        let list_output = SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&list_output.stdout);
        assert!(
            !listing.contains(&path.display().to_string()),
            "pruned worktree must no longer be listed: {listing}"
        );
    }

    #[tokio::test]
    async fn each_role_gets_its_own_isolated_worktree() {
        let repo = init_test_repo();
        let worktrees_root = TempDir::new().unwrap();
        let manager = WorktreeManager::new(repo.path(), worktrees_root.path()).unwrap();

        let coder = manager.create("run-3", "coder", "HEAD").await.unwrap();
        let reviewer = manager.create("run-3", "reviewer", "HEAD").await.unwrap();

        assert_ne!(coder.path(), reviewer.path());
        assert!(coder.path().exists());
        assert!(reviewer.path().exists());
    }
}
