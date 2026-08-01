//! Executes the actual push to `origin` -- the only place in the whole workspace that does this
//! (Architecture.md §10: "seul `warden-gated` a le droit de pousser vers `origin`").

use std::path::Path;

use tokio::process::Command;

use crate::error::{GatedError, Result};

/// Pushes `commit_sha` from the local bare gate repo to `origin`, updating `refs/heads/<branch>`
/// there.
pub async fn push_to_origin(bare_repo_path: &Path, commit_sha: &str, branch: &str) -> Result<()> {
    let refspec = format!("{commit_sha}:refs/heads/{branch}");
    let output = Command::new("git")
        .arg("-C")
        .arg(bare_repo_path)
        .args(["push", "origin", &refspec])
        .output()
        .await?;

    if !output.status.success() {
        return Err(GatedError::PushFailed {
            command: format!("git -C {} push origin {refspec}", bare_repo_path.display()),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as SyncCommand;
    use tempfile::TempDir;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = SyncCommand::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_origin_and_gate_repo() -> (TempDir, TempDir, String) {
        let origin = TempDir::new().unwrap();
        run_git(origin.path(), &["init", "--bare", "--quiet"]);

        let seed = TempDir::new().unwrap();
        run_git(seed.path(), &["init", "--quiet"]);
        run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
        run_git(seed.path(), &["config", "user.name", "warden-test"]);
        std::fs::write(seed.path().join("file.txt"), "content\n").unwrap();
        run_git(seed.path(), &["add", "."]);
        run_git(seed.path(), &["commit", "--quiet", "-m", "seed commit"]);
        let commit_sha_output = SyncCommand::new("git")
            .current_dir(seed.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let commit_sha = String::from_utf8_lossy(&commit_sha_output.stdout)
            .trim()
            .to_string();

        let gate = TempDir::new().unwrap();
        run_git(
            gate.path(),
            &[
                "clone",
                "--bare",
                "--quiet",
                &seed.path().display().to_string(),
                ".",
            ],
        );
        run_git(
            gate.path(),
            &[
                "remote",
                "set-url",
                "origin",
                &origin.path().display().to_string(),
            ],
        );

        (origin, gate, commit_sha)
    }

    #[tokio::test]
    async fn pushes_the_commit_to_origin_under_the_target_branch() {
        let (origin, gate, commit_sha) = init_origin_and_gate_repo();

        push_to_origin(gate.path(), &commit_sha, "main")
            .await
            .unwrap();

        let log_output = SyncCommand::new("git")
            .current_dir(origin.path())
            .args(["log", "-1", "--format=%H", "refs/heads/main"])
            .output()
            .unwrap();
        let origin_head = String::from_utf8_lossy(&log_output.stdout)
            .trim()
            .to_string();
        assert_eq!(origin_head, commit_sha);
    }

    #[tokio::test]
    async fn a_failed_push_is_a_typed_error_with_the_git_stderr_attached() {
        let gate = TempDir::new().unwrap();
        run_git(gate.path(), &["init", "--bare", "--quiet"]);

        let result = push_to_origin(gate.path(), "deadbeef", "main").await;
        assert!(matches!(result, Err(GatedError::PushFailed { .. })));
    }
}
