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

use serde::Deserialize;
use tokio::process::Command;

use crate::ci_watcher::{CheckConclusion, CheckRun, CiProvider, PrLifecycle, PrStatus};
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

    /// The `owner/repo` this provider is scoped to -- needed by callers that
    /// compose a `pr_manager::FinalizeRequest` themselves (e.g. `run_tail`'s
    /// `run-tail`/`resume-watch` CLI dispatch) and so can't re-derive it
    /// independently without duplicating `new`'s own resolution.
    pub fn repo_slug(&self) -> &str {
        &self.repo_slug
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

impl CiProvider for GhProvider {
    /// Fetches `pr`'s lifecycle and CI check statuses in a single `gh`
    /// invocation. Read-only -- see `ci_watcher`'s top-level doc comment.
    async fn pr_status(&self, pr: &PrHandle) -> Result<PrStatus> {
        let output = run_gh(&[
            "pr",
            "view",
            &pr.number.to_string(),
            "--repo",
            &self.repo_slug,
            "--json",
            "state,statusCheckRollup",
        ])
        .await?;

        parse_pr_status_json(&String::from_utf8_lossy(&output.stdout))
    }
}

/// Wire shape of `gh pr view --json state,statusCheckRollup`'s stdout.
#[derive(Debug, Deserialize)]
struct RawPrView {
    state: String,
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Vec<RawCheckEntry>,
}

/// One `statusCheckRollup` entry. GitHub reports these in one of two
/// overlapping shapes depending on which API the reporting CI integration
/// uses: the newer Checks API (`status`/`conclusion`, `__typename:
/// "CheckRun"`) or the legacy commit Statuses API (`state`, `__typename:
/// "StatusContext"`) -- both are modeled here so either is recognized,
/// rather than assuming every CI integration uses the newer one.
#[derive(Debug, Deserialize)]
struct RawCheckEntry {
    name: String,
    /// Checks API: `"QUEUED"` / `"IN_PROGRESS"` / `"COMPLETED"` / etc.
    status: Option<String>,
    /// Checks API, only meaningful once `status == "COMPLETED"`:
    /// `"SUCCESS"` / `"FAILURE"` / `"NEUTRAL"` / `"SKIPPED"` / `"CANCELLED"`
    /// / `"TIMED_OUT"` / `"ACTION_REQUIRED"` / `"STALE"`.
    conclusion: Option<String>,
    /// Legacy Statuses API: `"SUCCESS"` / `"FAILURE"` / `"ERROR"` /
    /// `"PENDING"`.
    state: Option<String>,
    #[serde(rename = "detailsUrl")]
    details_url: Option<String>,
    #[serde(rename = "targetUrl")]
    target_url: Option<String>,
}

/// Parses `gh pr view --json state,statusCheckRollup`'s stdout into a
/// [`PrStatus`]. Pure/no I/O, so it's directly testable against fixture JSON
/// without a real `gh` invocation or GitHub credentials.
fn parse_pr_status_json(json: &str) -> Result<PrStatus> {
    let raw: RawPrView =
        serde_json::from_str(json).map_err(|error| GatedError::UnparsablePrStatusJson {
            json: json.to_string(),
            reason: error.to_string(),
        })?;

    let lifecycle = parse_pr_lifecycle(&raw.state)?;
    let checks = raw
        .status_check_rollup
        .iter()
        .map(parse_check_entry)
        .collect::<Result<Vec<_>>>()?;

    Ok(PrStatus { lifecycle, checks })
}

/// GitHub's `state` field is a closed set (`OPEN`/`CLOSED`/`MERGED`) -- any
/// other value is a boundary error, not a guess.
fn parse_pr_lifecycle(raw: &str) -> Result<PrLifecycle> {
    match raw {
        "OPEN" => Ok(PrLifecycle::Open),
        "MERGED" => Ok(PrLifecycle::Merged),
        "CLOSED" => Ok(PrLifecycle::Closed),
        other => Err(GatedError::UnknownPrLifecycle(other.to_string())),
    }
}

/// Classifies one `statusCheckRollup` entry, trying the Checks API shape
/// (`status`/`conclusion`) first and falling back to the legacy Statuses
/// API shape (`state`) -- see [`RawCheckEntry`]'s doc comment for why both
/// exist.
fn parse_check_entry(raw: &RawCheckEntry) -> Result<CheckRun> {
    let details_url = raw
        .details_url
        .clone()
        .or_else(|| raw.target_url.clone())
        .filter(|url| !url.is_empty());

    let conclusion = if let Some(status) = raw.status.as_deref() {
        if status != "COMPLETED" {
            CheckConclusion::Pending
        } else {
            match raw.conclusion.as_deref() {
                Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => CheckConclusion::Passed,
                Some("FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STALE") => {
                    CheckConclusion::Failed
                }
                other => {
                    return Err(GatedError::UnknownCheckConclusion(format!(
                        "check {:?}: completed with conclusion {other:?}",
                        raw.name
                    )))
                }
            }
        }
    } else if let Some(state) = raw.state.as_deref() {
        match state {
            "SUCCESS" => CheckConclusion::Passed,
            "FAILURE" | "ERROR" => CheckConclusion::Failed,
            "PENDING" => CheckConclusion::Pending,
            other => {
                return Err(GatedError::UnknownCheckConclusion(format!(
                    "check {:?}: legacy status state {other:?}",
                    raw.name
                )))
            }
        }
    } else {
        return Err(GatedError::MalformedCheckEntry(raw.name.clone()));
    };

    Ok(CheckRun {
        name: raw.name.clone(),
        conclusion,
        details_url,
    })
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

    // ---- parse_pr_status_json (issue #5) -----------------------------------
    //
    // Fixtures below mirror real `gh pr view --json state,statusCheckRollup`
    // output (captured against a live GitHub Actions-backed PR), not
    // hand-invented shapes -- see gh_provider's `RawCheckEntry` doc comment
    // for why both a Checks-API and a legacy Statuses-API entry are covered.

    #[test]
    fn parses_an_open_pr_with_a_completed_successful_check_run() {
        let json = r#"{
            "state": "OPEN",
            "statusCheckRollup": [
                {
                    "__typename": "CheckRun",
                    "name": "build (ubuntu-latest)",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                    "detailsUrl": "https://github.com/o/r/actions/runs/1/job/2",
                    "startedAt": "2026-07-09T14:02:44Z",
                    "completedAt": "2026-07-09T14:02:45Z",
                    "workflowName": "CI"
                }
            ]
        }"#;
        let status = parse_pr_status_json(json).unwrap();
        assert_eq!(status.lifecycle, PrLifecycle::Open);
        assert_eq!(status.checks.len(), 1);
        assert_eq!(status.checks[0].conclusion, CheckConclusion::Passed);
        assert_eq!(
            status.checks[0].details_url.as_deref(),
            Some("https://github.com/o/r/actions/runs/1/job/2")
        );
    }

    #[test]
    fn a_check_run_still_in_progress_is_pending_even_without_a_conclusion() {
        let json = r#"{
            "state": "OPEN",
            "statusCheckRollup": [
                {
                    "__typename": "CheckRun",
                    "name": "build (ubuntu-latest)",
                    "status": "IN_PROGRESS",
                    "conclusion": null,
                    "detailsUrl": "https://github.com/o/r/actions/runs/1/job/2"
                }
            ]
        }"#;
        let status = parse_pr_status_json(json).unwrap();
        assert_eq!(status.checks[0].conclusion, CheckConclusion::Pending);
    }

    #[test]
    fn a_completed_failed_check_run_is_failed() {
        let json = r#"{
            "state": "OPEN",
            "statusCheckRollup": [
                {
                    "__typename": "CheckRun",
                    "name": "build (ubuntu-latest)",
                    "status": "COMPLETED",
                    "conclusion": "FAILURE",
                    "detailsUrl": "https://github.com/o/r/actions/runs/1/job/2"
                }
            ]
        }"#;
        let status = parse_pr_status_json(json).unwrap();
        assert_eq!(status.checks[0].conclusion, CheckConclusion::Failed);
    }

    #[test]
    fn a_skipped_completed_check_run_is_passed_not_failed() {
        let json = r#"{
            "state": "OPEN",
            "statusCheckRollup": [
                {
                    "__typename": "CheckRun",
                    "name": "label-external",
                    "status": "COMPLETED",
                    "conclusion": "SKIPPED",
                    "detailsUrl": ""
                }
            ]
        }"#;
        let status = parse_pr_status_json(json).unwrap();
        assert_eq!(status.checks[0].conclusion, CheckConclusion::Passed);
        assert_eq!(
            status.checks[0].details_url, None,
            "an empty detailsUrl string must not be treated as a real URL"
        );
    }

    #[test]
    fn a_legacy_status_context_entry_is_parsed_from_its_state_field() {
        let json = r#"{
            "state": "OPEN",
            "statusCheckRollup": [
                {
                    "__typename": "StatusContext",
                    "name": "ci/circleci",
                    "state": "SUCCESS",
                    "targetUrl": "https://circleci.com/gh/o/r/1"
                }
            ]
        }"#;
        let status = parse_pr_status_json(json).unwrap();
        assert_eq!(status.checks[0].conclusion, CheckConclusion::Passed);
        assert_eq!(
            status.checks[0].details_url.as_deref(),
            Some("https://circleci.com/gh/o/r/1")
        );
    }

    #[test]
    fn a_merged_pr_state_parses_to_the_merged_lifecycle() {
        let json = r#"{"state": "MERGED", "statusCheckRollup": []}"#;
        assert_eq!(
            parse_pr_status_json(json).unwrap().lifecycle,
            PrLifecycle::Merged
        );
    }

    #[test]
    fn a_closed_pr_state_parses_to_the_closed_lifecycle() {
        let json = r#"{"state": "CLOSED", "statusCheckRollup": []}"#;
        assert_eq!(
            parse_pr_status_json(json).unwrap().lifecycle,
            PrLifecycle::Closed
        );
    }

    #[test]
    fn an_unknown_pr_state_is_a_typed_error_not_a_panic() {
        let json = r#"{"state": "BOGUS", "statusCheckRollup": []}"#;
        assert!(matches!(
            parse_pr_status_json(json),
            Err(GatedError::UnknownPrLifecycle(_))
        ));
    }

    #[test]
    fn an_unrecognized_check_shape_is_a_typed_error_not_a_panic() {
        let json = r#"{
            "state": "OPEN",
            "statusCheckRollup": [{"name": "mystery"}]
        }"#;
        assert!(matches!(
            parse_pr_status_json(json),
            Err(GatedError::MalformedCheckEntry(name)) if name == "mystery"
        ));
    }

    #[test]
    fn malformed_json_from_gh_is_a_typed_error_not_a_panic() {
        assert!(matches!(
            parse_pr_status_json("not json"),
            Err(GatedError::UnparsablePrStatusJson { .. })
        ));
    }
}
