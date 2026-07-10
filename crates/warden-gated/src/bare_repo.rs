//! Creates and configures the local bare gate repository (ADR-0002) that
//! `warden` pushes converged runs into, and that this crate later relays
//! from `origin`. Used by the `init-bare` CLI subcommand; a thin wrapper
//! around a couple of `git` invocations, no decision logic of its own.

use std::path::Path;

use tokio::process::Command;

use crate::error::{GatedError, Result};

/// Initializes a bare repo at `bare_repo_path` (idempotent: does nothing if
/// one already exists there) and, if `origin_url` is given, points its
/// `origin` remote at it. The `origin` remote's credentials themselves are
/// never handled here -- whatever the machine's git/SSH config already
/// provides at push time (Architecture.md §10).
pub async fn init(bare_repo_path: &Path, origin_url: Option<&str>) -> Result<()> {
    if !bare_repo_path.join("HEAD").exists() {
        tokio::fs::create_dir_all(bare_repo_path).await?;
        run_git(bare_repo_path, &["init", "--bare", "--quiet"]).await?;
    }

    if let Some(origin_url) = origin_url {
        // `git remote add` fails if `origin` is already configured (e.g. a
        // second `init-bare` run); check explicitly instead of swallowing
        // whatever error `add` returns, so a genuinely unexpected git
        // failure still surfaces (code-standards.md: no silent fallback).
        if remote_exists(bare_repo_path, "origin").await? {
            run_git(bare_repo_path, &["remote", "set-url", "origin", origin_url]).await?;
        } else {
            run_git(bare_repo_path, &["remote", "add", "origin", origin_url]).await?;
        }
    }

    Ok(())
}

/// Whether `remote_name` is already configured in `repo_path`.
async fn remote_exists(repo_path: &Path, remote_name: &str) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["remote"])
        .output()
        .await?;

    if !output.status.success() {
        return Err(GatedError::GitCommandFailed {
            command: format!("git -C {} remote", repo_path.display()),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let remotes = String::from_utf8_lossy(&output.stdout);
    Ok(remotes.lines().any(|line| line.trim() == remote_name))
}

async fn run_git(repo_path: &Path, args: &[&str]) -> Result<()> {
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as SyncCommand;
    use tempfile::TempDir;

    #[tokio::test]
    async fn init_creates_a_bare_repo() {
        let dir = TempDir::new().unwrap();
        let bare_repo_path = dir.path().join("gate.git");

        init(&bare_repo_path, None).await.unwrap();

        assert!(bare_repo_path.join("HEAD").exists());
        let output = SyncCommand::new("git")
            .args(["-C", &bare_repo_path.display().to_string()])
            .args(["rev-parse", "--is-bare-repository"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "true");
    }

    #[tokio::test]
    async fn init_configures_the_origin_remote_when_given_a_url() {
        let dir = TempDir::new().unwrap();
        let bare_repo_path = dir.path().join("gate.git");

        init(&bare_repo_path, Some("/tmp/fake-origin"))
            .await
            .unwrap();

        let output = SyncCommand::new("git")
            .args(["-C", &bare_repo_path.display().to_string()])
            .args(["remote", "get-url", "origin"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "/tmp/fake-origin"
        );
    }

    #[tokio::test]
    async fn init_is_idempotent_and_updates_the_origin_url_on_a_second_call() {
        let dir = TempDir::new().unwrap();
        let bare_repo_path = dir.path().join("gate.git");

        init(&bare_repo_path, Some("/tmp/first-origin"))
            .await
            .unwrap();
        init(&bare_repo_path, Some("/tmp/second-origin"))
            .await
            .unwrap();

        let output = SyncCommand::new("git")
            .args(["-C", &bare_repo_path.display().to_string()])
            .args(["remote", "get-url", "origin"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "/tmp/second-origin"
        );
    }
}
