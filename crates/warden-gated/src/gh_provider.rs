//! GitHub implementation of the `PrProvider` seam (ADR-0007, issue #4), via
//! the `gh` CLI. Deliberately thin: every method is a single `gh`
//! invocation plus output parsing. `gh`'s own already-authenticated session
//! is used as-is -- `warden-gated` never stores, reads, or logs GitHub
//! credentials itself (issue #4: "reuse the machine's existing `gh` auth;
//! NEVER hardcode or store credentials").
//!
//! A `glab`-backed `GlabProvider` implementing the same `PrProvider` trait
//! is the intended extension point for GitLab (deferred -- GitHub is the
//! priority provider).

use std::path::Path;
use std::process::Output;

use tokio::process::Command;

use crate::error::{GatedError, Result};
use crate::pr_manager::{OpenDraftParams, PrHandle, PrProvider};

/// GitHub PR provider: every call is scoped to one `owner/repo` via `gh`'s
/// `--repo` flag, resolved once at construction so individual PR actions
/// never need to re-derive it.
pub struct GhProvider {
    repo_slug: String,
}

impl GhProvider {
    /// Resolves the target `owner/repo` either from `repo_slug_override`
    /// (explicit config) or, if absent, from the bare gate repo's `origin`
    /// remote URL (the same remote `push::push_to_origin` pushes to).
    pub async fn new(bare_repo_path: &Path, repo_slug_override: Option<&str>) -> Result<Self> {
        let repo_slug = match repo_slug_override {
            Some(slug) => slug.to_string(),
            None => {
                let origin_url = remote_url(bare_repo_path, "origin").await?;
                parse_github_slug_from_remote_url(&origin_url)
                    .ok_or_else(|| GatedError::UnknownOriginRemote(origin_url.clone()))?
            }
        };
        Ok(Self { repo_slug })
    }
}

impl PrProvider for GhProvider {
    async fn open_draft(&self, params: &OpenDraftParams<'_>) -> Result<PrHandle> {
        let output = run_gh(&[
            "pr",
            "create",
            "--draft",
            "--repo",
            &self.repo_slug,
            "--base",
            params.base_branch,
            "--head",
            params.branch,
            "--title",
            params.title,
            "--body",
            params.body,
        ])
        .await?;

        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let number = parse_pr_number_from_url(&url)
            .ok_or_else(|| GatedError::UnparsablePrUrl(url.clone()))?;
        Ok(PrHandle { number })
    }

    async fn post_comment(&self, pr: &PrHandle, body: &str) -> Result<()> {
        run_gh(&[
            "pr",
            "comment",
            &pr.number.to_string(),
            "--repo",
            &self.repo_slug,
            "--body",
            body,
        ])
        .await?;
        Ok(())
    }

    async fn mark_ready(&self, pr: &PrHandle) -> Result<()> {
        run_gh(&[
            "pr",
            "ready",
            &pr.number.to_string(),
            "--repo",
            &self.repo_slug,
        ])
        .await?;
        Ok(())
    }

    async fn update_body(&self, pr: &PrHandle, body: &str) -> Result<()> {
        run_gh(&[
            "pr",
            "edit",
            &pr.number.to_string(),
            "--repo",
            &self.repo_slug,
            "--body",
            body,
        ])
        .await?;
        Ok(())
    }
}

/// Runs `gh <args>`, failing loudly (command, exit status, stderr attached)
/// rather than swallowing a non-zero exit -- issue #4: "fail with clear,
/// actionable errors (include the `gh`/`glab` command, exit status, stderr
/// on failure)".
async fn run_gh(args: &[&str]) -> Result<Output> {
    let output = Command::new("gh").args(args).output().await?;
    if !output.status.success() {
        return Err(GatedError::GhCommandFailed {
            command: format!("gh {}", args.join(" ")),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output)
}

async fn remote_url(repo_path: &Path, remote_name: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["remote", "get-url", remote_name])
        .output()
        .await?;

    if !output.status.success() {
        return Err(GatedError::GitCommandFailed {
            command: format!(
                "git -C {} remote get-url {remote_name}",
                repo_path.display()
            ),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Extracts `owner/repo` from a GitHub remote URL in any of the forms `git`
/// itself commonly produces (`https://`, `http://`, `git@host:`,
/// `ssh://git@host/`). Pure/no I/O so it's directly testable without a real
/// remote configured.
pub fn parse_github_slug_from_remote_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let path = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("ssh://git@github.com/"))
        .or_else(|| trimmed.strip_prefix("git@github.com:"))?;

    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Extracts the PR number from the URL `gh pr create` prints on success
/// (e.g. `https://github.com/owner/repo/pull/42`). Pure/no I/O.
pub fn parse_pr_number_from_url(url: &str) -> Option<u64> {
    url.trim().rsplit('/').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_remote_url() {
        assert_eq!(
            parse_github_slug_from_remote_url("https://github.com/prometek/warden.git"),
            Some("prometek/warden".to_string())
        );
    }

    #[test]
    fn parses_https_remote_url_without_git_suffix() {
        assert_eq!(
            parse_github_slug_from_remote_url("https://github.com/prometek/warden"),
            Some("prometek/warden".to_string())
        );
    }

    #[test]
    fn parses_ssh_shorthand_remote_url() {
        assert_eq!(
            parse_github_slug_from_remote_url("git@github.com:prometek/warden.git"),
            Some("prometek/warden".to_string())
        );
    }

    #[test]
    fn parses_ssh_url_form() {
        assert_eq!(
            parse_github_slug_from_remote_url("ssh://git@github.com/prometek/warden.git"),
            Some("prometek/warden".to_string())
        );
    }

    #[test]
    fn rejects_a_non_github_remote() {
        assert_eq!(
            parse_github_slug_from_remote_url("https://gitlab.com/prometek/warden.git"),
            None
        );
    }

    #[test]
    fn rejects_a_url_missing_the_repo_segment() {
        assert_eq!(
            parse_github_slug_from_remote_url("https://github.com/prometek"),
            None
        );
    }

    #[test]
    fn parses_the_pr_number_from_a_create_output_url() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/prometek/warden/pull/42"),
            Some(42)
        );
    }

    #[test]
    fn rejects_a_non_numeric_trailing_segment() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/prometek/warden"),
            None
        );
    }
}
