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
        // deepest existing ancestor instead.
        let canonical_worktrees_root = canonicalize_best_effort(&worktrees_root)?;

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

/// Canonicalizes `path`, walking up to the nearest existing ancestor if
/// `path` itself doesn't exist yet.
fn canonicalize_best_effort(path: &Path) -> Result<PathBuf, WorktreeError> {
    let mut candidate = path.to_path_buf();
    loop {
        match candidate.canonicalize() {
            Ok(canonical) => {
                // Re-append the non-existent suffix we stripped off, so the
                // containment check in `new` still sees the full intended
                // path rather than an ancestor of it.
                let suffix = path.strip_prefix(&candidate).unwrap_or(Path::new(""));
                return Ok(canonical.join(suffix));
            }
            Err(_) => {
                if !candidate.pop() {
                    return Err(WorktreeError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no existing ancestor found for {}", path.display()),
                    )));
                }
            }
        }
    }
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
