//! PR Manager (ADR-0007, issue #4): the three-action PR lifecycle
//! (`OpenDraft` / `PostCycleUpdate` / `Finalize`) that `warden-gated` owns
//! exclusively, plus the linked-issue detection and commit-trailer
//! formatting that support it.
//!
//! This module never talks to a PR provider's API/CLI directly -- that's
//! [`PrProvider`]'s job, implemented by `gh_provider::GhProvider` (GitHub)
//! today. A `glab`-backed implementation of the same trait is the intended
//! drop-in extension point for GitLab (deferred: GitHub is the priority
//! provider per Architecture.md's roadmap).
//!
//! Security boundary (ADR-0002/0006/0007, unchanged by this module):
//! `OpenDraft` and `PostCycleUpdate` only ever push a branch skeleton or
//! talk to the PR provider's *metadata* (title/body/comments) -- never
//! business code. `Finalize` is the only action that pushes real content,
//! and it does so by calling the exact same `gate::verify_and_authorize` +
//! `push::push_to_origin` path the git-push gate itself uses (see
//! `serve::handle_push_notification_line`) -- never a separate, weaker
//! check.

use std::path::Path;

use sqlx::SqlitePool;
use tokio::process::Command;
use warden_core::{Finding, FindingSource};

use crate::error::{GatedError, Result};
use crate::gate::verify_and_authorize;
use crate::push;
use crate::verify::{GateBlockReason, GateDecision};

// ---------------------------------------------------------------------------
// Linked-issue detection (pure)
// ---------------------------------------------------------------------------

/// Detects `fixes #123` / `closes #123` / `resolves #123` (any case) inside
/// a run's intent, per ADR-0007 ("liée à l'issue détectée dans l'intent").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedIssue {
    pub number: u64,
    /// The exact keyword as written in the intent (case preserved) -- reused
    /// verbatim in the generated PR body so GitHub's own auto-close-on-merge
    /// linking (itself case-insensitive) still recognizes it.
    pub keyword: String,
}

fn linked_issue_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(fixes|closes|resolves)\s+#(\d+)")
            .expect("linked-issue pattern is a fixed, valid regex")
    })
}

/// Scans `intent` for the first `fixes|closes|resolves #<n>` reference.
/// Returns `None` if the intent doesn't reference an issue -- `open_draft`
/// falls back to naming the PR from the intent instead (ADR-0007).
pub fn detect_linked_issue(intent: &str) -> Option<LinkedIssue> {
    let captures = linked_issue_pattern().captures(intent)?;
    let keyword = captures.get(1)?.as_str().to_string();
    let number = captures.get(2)?.as_str().parse().ok()?;
    Some(LinkedIssue { number, keyword })
}

// ---------------------------------------------------------------------------
// PR title/body generation (pure)
// ---------------------------------------------------------------------------

const MAX_GENERATED_TITLE_LEN: usize = 72;

/// Generates a PR title from an intent when nothing more specific is
/// available: the intent's first non-blank line, truncated to a sane
/// length. Fails loudly on a blank intent rather than inventing a
/// placeholder title (code-standards.md: no silent fallback).
pub fn generate_pr_title(intent: &str) -> Result<String> {
    let first_line = intent
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or(GatedError::EmptyIntent)?;

    if first_line.chars().count() <= MAX_GENERATED_TITLE_LEN {
        return Ok(first_line.to_string());
    }
    let truncated: String = first_line
        .chars()
        .take(MAX_GENERATED_TITLE_LEN.saturating_sub(1))
        .collect();
    Ok(format!("{truncated}…"))
}

/// Builds the draft PR body: the linked-issue reference (if any, so GitHub
/// auto-links/auto-closes it on merge), the intent verbatim, and a fixed
/// note marking this as a skeleton draft (ADR-0007: "aucun contenu métier
/// n'est poussé avant Finalize").
pub fn open_draft_pr_body(intent: &str, linked_issue: Option<&LinkedIssue>) -> String {
    let mut sections = Vec::new();
    if let Some(issue) = linked_issue {
        sections.push(format!("{} #{}", issue.keyword, issue.number));
    }
    sections.push(intent.trim().to_string());
    sections.push(
        "---\n_Opened automatically by Warden as a draft skeleton branch. Business code lands \
         only once this run converges (ADR-0007)._"
            .to_string(),
    );
    sections.join("\n\n")
}

// ---------------------------------------------------------------------------
// Commit trailers (pure) -- Architecture.md §5.3
// ---------------------------------------------------------------------------

/// Role of the commit's author agent, embedded in the `Warden-Agent`
/// trailer. Deliberately its own type rather than `warden_core::AgentRole`:
/// the doc agent also produces trailer-bearing commits (Architecture.md
/// §5.3), but `AgentRole` (`RUNS`/`AGENT_PROCESSES` domain) only models the
/// roles that run *during* a cycle (coder/reviewer/tester) -- stretching it
/// to cover doc would blur that table's meaning for an unrelated concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailerAgent {
    Coder,
    Doc,
}

impl TrailerAgent {
    pub fn as_str(self) -> &'static str {
        match self {
            TrailerAgent::Coder => "coder",
            TrailerAgent::Doc => "doc",
        }
    }
}

/// The three structured commit trailers coder/doc commits carry locally
/// (Architecture.md §5.3) -- no remote access needed to produce these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitTrailers {
    pub cycle: u32,
    pub findings_resolved: Vec<String>,
    pub agent: TrailerAgent,
}

impl CommitTrailers {
    /// Renders just the trailer block, one `Key: value` line per trailer, in
    /// the order shown in Architecture.md §5.3's table.
    /// `Warden-Findings-Resolved` is omitted entirely when empty (a coder's
    /// very first commit in a cycle may resolve nothing yet) rather than
    /// emitted with an empty value.
    pub fn format(&self) -> String {
        let mut lines = vec![format!("Warden-Cycle: {}", self.cycle)];
        if !self.findings_resolved.is_empty() {
            lines.push(format!(
                "Warden-Findings-Resolved: {}",
                self.findings_resolved.join(", ")
            ));
        }
        lines.push(format!("Warden-Agent: {}", self.agent.as_str()));
        lines.join("\n")
    }
}

/// Appends `trailers` to `commit_message`, separated by the blank line git
/// trailers require. Purely a string transform -- the caller still performs
/// the actual local `git commit` (no I/O here, no remote access needed).
pub fn append_trailers(commit_message: &str, trailers: &CommitTrailers) -> String {
    format!("{}\n\n{}\n", commit_message.trim_end(), trailers.format())
}

// ---------------------------------------------------------------------------
// Cycle comment formatting (pure)
// ---------------------------------------------------------------------------

/// One cycle's reviewer/tester findings, as already parsed by
/// `warden_core::parse_findings` -- `post_cycle_update` only formats and
/// posts them, it never re-derives or re-validates finding content itself.
#[derive(Debug, Clone)]
pub struct CycleSummary {
    pub cycle_number: u32,
    pub findings: Vec<Finding>,
}

/// Renders one cycle's findings into the PR comment body `post_cycle_update`
/// posts. Purely informational formatting -- posting it never touches PR
/// status or content (that boundary is enforced by `post_cycle_update` only
/// ever calling `PrProvider::post_comment`, never `mark_ready`/
/// `update_body`).
pub fn format_cycle_comment(summary: &CycleSummary) -> String {
    let mut body = format!("## Warden — cycle {} update\n\n", summary.cycle_number);

    if summary.findings.is_empty() {
        body.push_str("No findings raised this cycle.\n\n");
    } else {
        for source in [FindingSource::Reviewer, FindingSource::Tester] {
            let from_source: Vec<&Finding> = summary
                .findings
                .iter()
                .filter(|finding| finding.source == source)
                .collect();
            if from_source.is_empty() {
                continue;
            }
            body.push_str(&format!("**{}**\n\n", title_case(source.as_str())));
            for finding in from_source {
                body.push_str(&format_finding_line(finding));
                body.push('\n');
            }
            body.push('\n');
        }
    }

    body.push_str("_Informational only — does not change this PR's draft status or content._\n");
    body
}

fn format_finding_line(finding: &Finding) -> String {
    let location = finding
        .file
        .as_deref()
        .map(|file| format!(" ({file})"))
        .unwrap_or_default();
    let action = finding
        .action
        .as_deref()
        .map(|action| format!(" — suggested: {action}"))
        .unwrap_or_default();
    format!(
        "- [{}]{location} {}{action}",
        finding.severity.as_str(),
        finding.description
    )
}

fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Provider seam
// ---------------------------------------------------------------------------

/// Provider-agnostic handle to an already-opened PR: everything
/// `post_cycle_update`/`finalize` need to address it again. `GhProvider`
/// scopes itself to one `owner/repo` at construction, so this only carries
/// the provider-native PR number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrHandle {
    pub number: u64,
}

/// Everything `PrProvider::open_draft` needs to open the draft PR.
pub struct OpenDraftParams<'a> {
    pub branch: &'a str,
    pub base_branch: &'a str,
    pub title: &'a str,
    pub body: &'a str,
}

/// Thin seam over a PR provider's CLI. GitHub ships first via
/// `gh_provider::GhProvider` (Architecture.md's roadmap: "Provider CI/PR
/// prioritaire"). A `glab`-backed implementation is a drop-in second impl of
/// this same trait; nothing in this module's orchestration functions is
/// GitHub-specific.
///
/// `async fn` in this trait is intentional, not an oversight: every call
/// site (`open_draft`/`post_cycle_update`/`finalize`) awaits a `PrProvider`
/// directly on its own task rather than boxing it into a `dyn` trait object
/// or handing it to `tokio::spawn`, so the `Send`-bound future the compiler
/// would otherwise require is unnecessary here.
#[allow(async_fn_in_trait)]
pub trait PrProvider {
    /// Opens a **draft** PR for `params.branch` against `params.base_branch`.
    async fn open_draft(&self, params: &OpenDraftParams<'_>) -> Result<PrHandle>;
    /// Posts an informational comment. Must never change draft status or body.
    async fn post_comment(&self, pr: &PrHandle, body: &str) -> Result<()>;
    /// Flips a PR from draft to ready for review.
    async fn mark_ready(&self, pr: &PrHandle) -> Result<()>;
    /// Replaces the PR's body.
    async fn update_body(&self, pr: &PrHandle, body: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// OpenDraft
// ---------------------------------------------------------------------------

/// Everything `open_draft` needs: the skeleton commit to push and the
/// metadata to open the draft PR from.
pub struct OpenDraftRequest<'a> {
    /// The local bare gate repo to push the skeleton branch from (the same
    /// repo `push::push_to_origin` always pushes from).
    pub bare_repo_path: &'a Path,
    /// The branch-skeleton commit -- **must** contain no business code.
    /// `open_draft` never takes the caller's word for that: it independently
    /// re-derives it (see `skeleton_is_content_free`) before ever pushing,
    /// the same "never trust the caller" principle `gate::verify_and_authorize`
    /// applies to convergence (ADR-0002/0006).
    pub skeleton_commit_sha: &'a str,
    pub branch: &'a str,
    pub base_branch: &'a str,
    pub intent: &'a str,
}

/// Independently determines whether `skeleton_commit_sha` changes anything
/// relative to `base_branch`'s current tip on `origin`. Returns the list of
/// changed file paths across the *whole* range that would actually be
/// transferred by `git push {skeleton}:refs/heads/{branch}` (empty means
/// content-free).
///
/// `open_draft` must never trust a caller-supplied sha to truly be "just a
/// branch skeleton" (issue #4 review, finding #1) -- this re-derives that
/// fact itself instead, mirroring how `gate::verify_and_authorize` re-derives
/// convergence rather than trusting the caller. Fetching `base_branch` from
/// `origin` is a read-only operation already within the access
/// `warden-gated` holds (it can already push there) -- no new credential
/// exposure.
///
/// Checking only the *endpoint* diff (skeleton tip tree vs. base tip tree)
/// is not enough (issue #4 review, follow-up finding): `git push` transfers
/// every commit's objects in the pushed range, not just the net difference
/// between the two ends. A skeleton whose tip tree matches base, but whose
/// history contains an intermediate commit that adds a business file and a
/// later one that removes it again, would pass an endpoint-only check while
/// still landing those blobs on `origin`, reachable forever via `git
/// log`/`git show`. So every commit that would be pushed is checked
/// individually, against its own parent (or the empty tree, for a root
/// commit) -- not just the range's net effect.
async fn skeleton_diff_against_base(
    bare_repo_path: &Path,
    base_branch: &str,
    skeleton_commit_sha: &str,
) -> Result<Vec<String>> {
    let base_sha = match remote_branch_head(bare_repo_path, "origin", base_branch).await? {
        Some(base_sha) => {
            fetch_branch(bare_repo_path, "origin", base_branch).await?;
            Some(base_sha)
        }
        // `base_branch` doesn't exist on `origin` yet -- there's nothing to
        // exclude, so every commit reachable from the skeleton is in scope.
        None => None,
    };

    let pushed_commits =
        commits_in_range(bare_repo_path, base_sha.as_deref(), skeleton_commit_sha).await?;

    let mut offending_files = Vec::new();
    for commit in &pushed_commits {
        offending_files.extend(commit_own_diff(bare_repo_path, commit).await?);
    }
    offending_files.sort();
    offending_files.dedup();
    Ok(offending_files)
}

/// The sha `base_branch` currently points to on `remote`, or `None` if that
/// branch doesn't exist there yet.
///
/// Lists *every* head and matches the ref column exactly against
/// `refs/heads/{branch}`, rather than handing `branch` to `git ls-remote` as
/// a pattern: `git ls-remote --heads <remote> <branch>` matches any ref
/// whose path ends in `/branch` (e.g. `refs/heads/feat/main` alongside
/// `refs/heads/main`), and taking the first output line unconditionally
/// could silently pick a sibling branch's sha as the "base" (issue #4
/// review, follow-up finding).
async fn remote_branch_head(
    repo_path: &Path,
    remote: &str,
    branch: &str,
) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["ls-remote", "--heads", remote])
        .output()
        .await?;

    if !output.status.success() {
        return Err(GatedError::GitCommandFailed {
            command: format!("git -C {} ls-remote --heads {remote}", repo_path.display()),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let target_ref = format!("refs/heads/{branch}");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let sha = fields.next()?;
            let refname = fields.next()?;
            (refname == target_ref).then(|| sha.to_string())
        }))
}

/// Fetches `branch` from `remote` into `repo_path`'s local object store
/// (read-only) so its history can be walked/diffed locally.
async fn fetch_branch(repo_path: &Path, remote: &str, branch: &str) -> Result<()> {
    run_git(repo_path, &["fetch", "--quiet", remote, branch]).await
}

/// Lists the commits that `git push {range_end}:refs/heads/<branch>` would
/// actually transfer: everything reachable from `range_end` but not from
/// `range_start` (`git rev-list <start>..<end>`), or -- when there is no
/// `range_start` at all (`base_branch` doesn't exist on `origin` yet) --
/// every commit reachable from `range_end`, since there's nothing to
/// exclude.
async fn commits_in_range(
    repo_path: &Path,
    range_start: Option<&str>,
    range_end: &str,
) -> Result<Vec<String>> {
    let range_arg = match range_start {
        Some(start) => format!("{start}..{range_end}"),
        None => range_end.to_string(),
    };
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-list", &range_arg])
        .output()
        .await?;
    if !output.status.success() {
        return Err(GatedError::GitCommandFailed {
            command: format!("git -C {} rev-list {range_arg}", repo_path.display()),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// The file paths `commit_sha` itself introduces, relative to its first
/// parent -- or relative to the empty tree if it's a root commit (`--root`,
/// so a root commit's real content isn't mistaken for "no diff" just
/// because it has no parent to diff against).
async fn commit_own_diff(repo_path: &Path, commit_sha: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
            commit_sha,
        ])
        .output()
        .await?;
    if !output.status.success() {
        return Err(GatedError::GitCommandFailed {
            command: format!(
                "git -C {} diff-tree --no-commit-id --name-only -r --root {commit_sha}",
                repo_path.display()
            ),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
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

/// `OpenDraft` (ADR-0007): pushes only the branch skeleton to `origin`, then
/// opens a draft PR linked to the issue the intent references (or titled
/// from the intent otherwise). Triggered at coder start -- before any
/// business code exists, so this is the earliest point metadata is allowed
/// to reach `origin` under ADR-0002/0007.
///
/// Order matters here (issue #4 review, finding #2): all fallible *pure*
/// validation/generation (title, body) runs first, so a caller mistake
/// (e.g. a blank intent) surfaces before anything irreversible happens.
/// Only once that's settled does this reach for I/O -- first the
/// independent content-free re-verification (finding #1), then the push
/// itself, and only if both succeed does it ask the provider to open the PR
/// (which itself needs the branch already on `origin` to set `--head`).
///
/// Trigger wiring: how `warden` actually invokes this action (CLI
/// subcommand vs. some other channel) is a separate architectural decision
/// deferred out of this module's scope -- see issue #4.
pub async fn open_draft<P: PrProvider>(
    request: &OpenDraftRequest<'_>,
    provider: &P,
) -> Result<PrHandle> {
    let linked_issue = detect_linked_issue(request.intent);
    let title = generate_pr_title(request.intent)?;
    let body = open_draft_pr_body(request.intent, linked_issue.as_ref());

    let offending_files = skeleton_diff_against_base(
        request.bare_repo_path,
        request.base_branch,
        request.skeleton_commit_sha,
    )
    .await?;
    if !offending_files.is_empty() {
        return Err(GatedError::SkeletonNotContentFree {
            commit_sha: request.skeleton_commit_sha.to_string(),
            base_branch: request.base_branch.to_string(),
            files: offending_files,
        });
    }

    push::push_to_origin(
        request.bare_repo_path,
        request.skeleton_commit_sha,
        request.branch,
    )
    .await?;

    provider
        .open_draft(&OpenDraftParams {
            branch: request.branch,
            base_branch: request.base_branch,
            title: &title,
            body: &body,
        })
        .await
}

// ---------------------------------------------------------------------------
// PostCycleUpdate
// ---------------------------------------------------------------------------

/// `PostCycleUpdate` (ADR-0007): posts one cycle's findings as a PR comment.
/// Purely informational -- only ever calls `PrProvider::post_comment`, never
/// touches the PR's draft status or body.
pub async fn post_cycle_update<P: PrProvider>(
    pr: &PrHandle,
    summary: &CycleSummary,
    provider: &P,
) -> Result<()> {
    let body = format_cycle_comment(summary);
    provider.post_comment(pr, &body).await
}

// ---------------------------------------------------------------------------
// Finalize
// ---------------------------------------------------------------------------

/// Outcome of a `Finalize` attempt -- mirrors [`GateDecision`] (the exact
/// same authorization result `finalize` re-derives), but named for this call
/// site so a blocked finalize reads as "blocked", not "should have pushed
/// but silently didn't".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeOutcome {
    Finalized { commit_sha: String },
    Blocked(GateBlockReason),
}

/// Everything `finalize` needs beyond the read-only database pool (bundled
/// into a struct rather than passed as separate arguments -- mirrors
/// `OpenDraftRequest`, and keeps the call site readable).
pub struct FinalizeRequest<'a> {
    /// The local bare gate repo to push the final content from.
    pub bare_repo_path: &'a Path,
    pub branch: &'a str,
    pub run_id: &'a str,
    /// The commit that was actually written into the bare gate repo --
    /// checked against `runs.converged_commit_sha`, never trusted as-is.
    pub pushed_commit_sha: &'a str,
    pub pr: &'a PrHandle,
    /// The full run summary to write into the PR body -- composed by the
    /// caller (the orchestrator, which has access to the run's
    /// cycles/findings history); this module only knows how to post it, the
    /// same way `post_cycle_update` doesn't re-derive findings itself.
    pub summary_body: &'a str,
}

/// `Finalize` (ADR-0007): re-verifies `state == Converged` and the committed
/// hash via the exact same path the git-push gate itself uses
/// (`gate::verify_and_authorize`, see `serve::handle_push_notification_line`
/// -- deliberately not reimplemented here), and only if authorized: pushes
/// the final content, updates the PR body with the full summary, and removes
/// draft status. Order matters -- the body is updated *before* the PR is
/// marked ready, so a reviewer can never see a "ready" PR with a stale body,
/// even momentarily.
///
/// Partial-failure note (issue #4 review, finding #3): if the push succeeds
/// but `update_body` or `mark_ready` then fails (e.g. a transient `gh`
/// error), the PR is left as a **draft carrying the previous or
/// partially-updated body** -- never "ready" with a stale body, since
/// `mark_ready` only runs after `update_body` completes. `push_to_origin`,
/// `gh pr edit`, and `gh pr ready` are all idempotent, so simply retrying
/// `finalize` with the same `FinalizeRequest` converges to the fully
/// finalized state without any special-cased recovery logic here.
pub async fn finalize<P: PrProvider>(
    pool: &SqlitePool,
    request: &FinalizeRequest<'_>,
    provider: &P,
) -> Result<FinalizeOutcome> {
    let decision = verify_and_authorize(pool, request.run_id, request.pushed_commit_sha).await?;

    match decision {
        GateDecision::Blocked(reason) => Ok(FinalizeOutcome::Blocked(reason)),
        GateDecision::Allow { commit_sha } => {
            push::push_to_origin(request.bare_repo_path, &commit_sha, request.branch).await?;
            provider
                .update_body(request.pr, request.summary_body)
                .await?;
            provider.mark_ready(request.pr).await?;
            Ok(FinalizeOutcome::Finalized { commit_sha })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use warden_core::Severity;

    // ---- detect_linked_issue -------------------------------------------

    #[test]
    fn detects_fixes_keyword_case_insensitively() {
        let linked = detect_linked_issue("FIXES #123: handle token expiry").unwrap();
        assert_eq!(linked.number, 123);
        assert_eq!(linked.keyword, "FIXES");
    }

    #[test]
    fn detects_closes_and_resolves_keywords() {
        assert_eq!(detect_linked_issue("closes #7").unwrap().number, 7);
        assert_eq!(
            detect_linked_issue("Resolves #42 today").unwrap().number,
            42
        );
    }

    #[test]
    fn finds_the_reference_anywhere_in_a_multi_line_intent() {
        let intent = "Add JWT expiry handling.\n\nFixes #99\n\nAlso cleans up logging.";
        assert_eq!(detect_linked_issue(intent).unwrap().number, 99);
    }

    #[test]
    fn returns_none_when_the_intent_does_not_reference_an_issue() {
        assert!(detect_linked_issue("Add JWT expiry handling").is_none());
    }

    #[test]
    fn does_not_match_a_bare_issue_number_without_a_keyword() {
        assert!(detect_linked_issue("see #123 for context").is_none());
    }

    #[test]
    fn does_not_match_fixes_without_a_hash() {
        assert!(detect_linked_issue("fixes 123").is_none());
    }

    // ---- generate_pr_title ----------------------------------------------

    #[test]
    fn generates_a_title_from_the_first_non_blank_line() {
        let title = generate_pr_title("\n  Add JWT expiry handling\nmore detail below").unwrap();
        assert_eq!(title, "Add JWT expiry handling");
    }

    #[test]
    fn truncates_an_overly_long_first_line() {
        let long_line = "a".repeat(200);
        let title = generate_pr_title(&long_line).unwrap();
        assert_eq!(title.chars().count(), MAX_GENERATED_TITLE_LEN);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn rejects_a_blank_intent_rather_than_inventing_a_title() {
        assert!(matches!(
            generate_pr_title("   \n  \n"),
            Err(GatedError::EmptyIntent)
        ));
    }

    // ---- open_draft_pr_body ----------------------------------------------

    #[test]
    fn body_leads_with_the_issue_reference_when_linked() {
        let linked = LinkedIssue {
            number: 123,
            keyword: "Fixes".to_string(),
        };
        let body = open_draft_pr_body("Handle token expiry", Some(&linked));
        assert!(body.starts_with("Fixes #123"));
        assert!(body.contains("Handle token expiry"));
    }

    #[test]
    fn body_has_no_issue_reference_when_none_is_linked() {
        let body = open_draft_pr_body("Handle token expiry", None);
        assert!(!body.contains('#'));
    }

    // ---- trailer formatting -----------------------------------------------

    #[test]
    fn formats_trailers_matching_the_architecture_doc_example() {
        let trailers = CommitTrailers {
            cycle: 3,
            findings_resolved: vec!["r-042".to_string()],
            agent: TrailerAgent::Coder,
        };
        assert_eq!(
            trailers.format(),
            "Warden-Cycle: 3\nWarden-Findings-Resolved: r-042\nWarden-Agent: coder"
        );
    }

    #[test]
    fn omits_findings_resolved_trailer_when_nothing_was_resolved_yet() {
        let trailers = CommitTrailers {
            cycle: 1,
            findings_resolved: vec![],
            agent: TrailerAgent::Coder,
        };
        assert_eq!(trailers.format(), "Warden-Cycle: 1\nWarden-Agent: coder");
    }

    #[test]
    fn joins_multiple_resolved_finding_ids() {
        let trailers = CommitTrailers {
            cycle: 2,
            findings_resolved: vec!["r-001".to_string(), "t-002".to_string()],
            agent: TrailerAgent::Doc,
        };
        assert_eq!(
            trailers.format(),
            "Warden-Cycle: 2\nWarden-Findings-Resolved: r-001, t-002\nWarden-Agent: doc"
        );
    }

    #[test]
    fn append_trailers_matches_the_full_architecture_doc_example() {
        let message = "fix: gère le cas d'expiration du token JWT\n\n\
            Corrige le finding remonté par le reviewer sur l'absence de\n\
            vérification d'expiration côté middleware auth.";
        let trailers = CommitTrailers {
            cycle: 3,
            findings_resolved: vec!["r-042".to_string()],
            agent: TrailerAgent::Coder,
        };

        let full = append_trailers(message, &trailers);

        assert_eq!(
            full,
            "fix: gère le cas d'expiration du token JWT\n\n\
            Corrige le finding remonté par le reviewer sur l'absence de\n\
            vérification d'expiration côté middleware auth.\n\n\
            Warden-Cycle: 3\nWarden-Findings-Resolved: r-042\nWarden-Agent: coder\n"
        );
    }

    #[test]
    fn append_trailers_trims_trailing_whitespace_before_the_blank_separator() {
        let trailers = CommitTrailers {
            cycle: 1,
            findings_resolved: vec![],
            agent: TrailerAgent::Coder,
        };
        let full = append_trailers("fix: something\n\n\n", &trailers);
        assert_eq!(
            full,
            "fix: something\n\nWarden-Cycle: 1\nWarden-Agent: coder\n"
        );
    }

    // ---- format_cycle_comment -----------------------------------------------

    fn reviewer_finding() -> Finding {
        Finding {
            source: FindingSource::Reviewer,
            severity: Severity::Blocking,
            file: Some("src/auth.rs".to_string()),
            description: "missing expiry check".to_string(),
            action: Some("add expiry check".to_string()),
        }
    }

    fn tester_finding() -> Finding {
        Finding {
            source: FindingSource::Tester,
            severity: Severity::Info,
            file: None,
            description: "consider adding an e2e test".to_string(),
            action: None,
        }
    }

    #[test]
    fn comment_lists_findings_grouped_by_source() {
        let summary = CycleSummary {
            cycle_number: 3,
            findings: vec![reviewer_finding(), tester_finding()],
        };
        let comment = format_cycle_comment(&summary);
        assert!(comment.contains("cycle 3"));
        assert!(comment.contains("**Reviewer**"));
        assert!(comment.contains("missing expiry check"));
        assert!(comment.contains("(src/auth.rs)"));
        assert!(comment.contains("**Tester**"));
        assert!(comment.contains("consider adding an e2e test"));
    }

    #[test]
    fn comment_says_so_explicitly_when_no_findings_were_raised() {
        let summary = CycleSummary {
            cycle_number: 1,
            findings: vec![],
        };
        let comment = format_cycle_comment(&summary);
        assert!(comment.contains("No findings raised this cycle."));
    }

    #[test]
    fn comment_always_states_it_is_informational_only() {
        let summary = CycleSummary {
            cycle_number: 1,
            findings: vec![],
        };
        let comment = format_cycle_comment(&summary);
        assert!(comment.contains("Informational only"));
    }

    // ---- orchestration functions -------------------------------------------

    /// In-memory `PrProvider` recording every call it receives -- stands in
    /// for a real PR provider so these tests exercise `pr_manager`'s
    /// orchestration logic without ever invoking `gh`/network I/O
    /// (code-standards.md: "pas d'appel réseau externe").
    struct RecordingProvider {
        calls: std::sync::Mutex<Vec<String>>,
        next_pr_number: u64,
    }

    impl RecordingProvider {
        fn new(next_pr_number: u64) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                next_pr_number,
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PrProvider for RecordingProvider {
        async fn open_draft(&self, params: &OpenDraftParams<'_>) -> Result<PrHandle> {
            self.calls.lock().unwrap().push(format!(
                "open_draft({}, {}) body={}",
                params.branch, params.title, params.body
            ));
            Ok(PrHandle {
                number: self.next_pr_number,
            })
        }

        async fn post_comment(&self, pr: &PrHandle, body: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("post_comment({}, {body})", pr.number));
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

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// The exact-match fix (issue #4 review, follow-up finding): `origin`
    /// has both `refs/heads/main` and `refs/heads/feat/main` (a sibling
    /// branch whose last path segment also happens to be "main"). Asking
    /// for `main` must resolve to `refs/heads/main`'s own sha, never
    /// `feat/main`'s -- `git ls-remote --heads <remote> main` alone would
    /// match both refs as a pattern, so this exercises `remote_branch_head`
    /// doing the exact-ref filtering itself rather than trusting the
    /// pattern match.
    #[tokio::test]
    async fn remote_branch_head_matches_the_exact_ref_not_a_sibling_suffix_match() {
        let origin = tempfile::TempDir::new().unwrap();
        run_git(origin.path(), &["init", "--bare", "--quiet"]);

        let seed = tempfile::TempDir::new().unwrap();
        run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
        run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
        run_git(seed.path(), &["config", "user.name", "warden-test"]);
        run_git(
            seed.path(),
            &["commit", "--quiet", "--allow-empty", "-m", "main tip"],
        );
        let main_sha = {
            let output = std::process::Command::new("git")
                .current_dir(seed.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        run_git(
            seed.path(),
            &[
                "push",
                "--quiet",
                &origin.path().display().to_string(),
                "main",
            ],
        );

        run_git(seed.path(), &["checkout", "--quiet", "-b", "feat/main"]);
        run_git(
            seed.path(),
            &["commit", "--quiet", "--allow-empty", "-m", "feat/main tip"],
        );
        run_git(
            seed.path(),
            &[
                "push",
                "--quiet",
                &origin.path().display().to_string(),
                "feat/main",
            ],
        );

        let gate_repo = tempfile::TempDir::new().unwrap();
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

        let head = remote_branch_head(gate_repo.path(), "origin", "main")
            .await
            .unwrap();
        assert_eq!(
            head,
            Some(main_sha),
            "must resolve refs/heads/main exactly, not a sibling refs/heads/feat/main"
        );
    }

    /// A bare gate repo with `origin` pointed at a second local bare repo,
    /// plus the sha of a commit already present in the gate repo -- enough
    /// to exercise `push::push_to_origin` (which `open_draft`/`finalize`
    /// both call) without any network access.
    ///
    /// `origin`'s `main` branch is seeded with this exact commit as its tip
    /// (pushed there before the fixture returns): `open_draft`'s content-
    /// free check (`skeleton_diff_against_base`) fetches `main` from
    /// `origin` and diffs the skeleton against it, so the returned commit
    /// must genuinely be content-free relative to whatever `origin` already
    /// has -- matching real usage, where `OpenDraft` fires before the coder
    /// has changed anything.
    fn gate_repo_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        tempfile::TempDir,
        String,
    ) {
        let origin = tempfile::TempDir::new().unwrap();
        run_git(origin.path(), &["init", "--bare", "--quiet"]);

        let seed = tempfile::TempDir::new().unwrap();
        run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
        run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
        run_git(seed.path(), &["config", "user.name", "warden-test"]);
        std::fs::write(seed.path().join("f.txt"), "skeleton\n").unwrap();
        run_git(seed.path(), &["add", "."]);
        run_git(seed.path(), &["commit", "--quiet", "-m", "skeleton"]);
        let commit_sha = {
            let output = std::process::Command::new("git")
                .current_dir(seed.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        run_git(
            seed.path(),
            &[
                "push",
                "--quiet",
                &origin.path().display().to_string(),
                "main",
            ],
        );

        let gate_repo = tempfile::TempDir::new().unwrap();
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

        (origin, seed, gate_repo, commit_sha)
    }

    /// A bare gate repo whose local history has a skeleton commit followed
    /// by a second "business code" commit on top of it (standing in for
    /// content that -- through some later bug or race -- ended up in the
    /// gate's local history ahead of `Finalize`). `open_draft` must still
    /// only ever push the exact sha it's handed, never the branch tip, so
    /// this lets tests prove that invariant even when business code already
    /// exists locally.
    ///
    /// Only the skeleton commit is pushed to `origin`'s `main` -- the
    /// business commit stays local-only, standing in for content that must
    /// never reach `origin` via `OpenDraft`. Returns `(origin, gate_repo,
    /// skeleton_sha, business_sha)`.
    fn gate_repo_with_business_commit_on_top_of_skeleton(
    ) -> (tempfile::TempDir, tempfile::TempDir, String, String) {
        let origin = tempfile::TempDir::new().unwrap();
        run_git(origin.path(), &["init", "--bare", "--quiet"]);

        let seed = tempfile::TempDir::new().unwrap();
        run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
        run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
        run_git(seed.path(), &["config", "user.name", "warden-test"]);
        std::fs::write(seed.path().join("SKELETON.md"), "skeleton\n").unwrap();
        run_git(seed.path(), &["add", "."]);
        run_git(seed.path(), &["commit", "--quiet", "-m", "skeleton"]);
        let skeleton_sha = {
            let output = std::process::Command::new("git")
                .current_dir(seed.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        run_git(
            seed.path(),
            &[
                "push",
                "--quiet",
                &origin.path().display().to_string(),
                "main",
            ],
        );

        std::fs::write(seed.path().join("src_business.rs"), "fn business() {}\n").unwrap();
        run_git(seed.path(), &["add", "."]);
        run_git(seed.path(), &["commit", "--quiet", "-m", "business code"]);
        let business_sha = {
            let output = std::process::Command::new("git")
                .current_dir(seed.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };

        let gate_repo = tempfile::TempDir::new().unwrap();
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

        (origin, gate_repo, skeleton_sha, business_sha)
    }

    #[tokio::test]
    async fn open_draft_pushes_the_skeleton_and_opens_a_draft_pr() {
        let (origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
        let provider = RecordingProvider::new(7);

        let request = OpenDraftRequest {
            bare_repo_path: gate_repo.path(),
            skeleton_commit_sha: &commit_sha,
            branch: "warden/run-1",
            base_branch: "main",
            intent: "Fixes #123: handle token expiry",
        };

        let pr = open_draft(&request, &provider).await.unwrap();
        assert_eq!(pr.number, 7);

        let calls = provider.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with("open_draft(warden/run-1,"));
        assert!(calls[0].contains("Fixes #123"));

        let output = std::process::Command::new("git")
            .current_dir(origin.path())
            .args(["log", "-1", "--format=%H", "refs/heads/warden/run-1"])
            .output()
            .unwrap();
        let origin_head = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(origin_head, commit_sha, "skeleton commit must reach origin");
    }

    #[tokio::test]
    async fn open_draft_falls_back_to_an_intent_derived_title_when_no_issue_is_linked() {
        let (_origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
        let provider = RecordingProvider::new(9);

        let intent = "Add JWT expiry handling";
        let request = OpenDraftRequest {
            bare_repo_path: gate_repo.path(),
            skeleton_commit_sha: &commit_sha,
            branch: "warden/run-2",
            base_branch: "main",
            intent,
        };

        let pr = open_draft(&request, &provider).await.unwrap();
        assert_eq!(pr.number, 9);

        let calls = provider.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with(&format!("open_draft(warden/run-2, {intent})")));
        assert!(
            !calls[0].contains('#'),
            "no linked issue means neither the generated title nor the body may \
             reference one: {}",
            calls[0]
        );
    }

    #[tokio::test]
    async fn open_draft_never_pushes_content_beyond_the_given_skeleton_commit() {
        let (origin, gate_repo, skeleton_sha, _business_sha) =
            gate_repo_with_business_commit_on_top_of_skeleton();
        let provider = RecordingProvider::new(11);

        let request = OpenDraftRequest {
            bare_repo_path: gate_repo.path(),
            skeleton_commit_sha: &skeleton_sha,
            branch: "warden/run-3",
            base_branch: "main",
            intent: "Add a feature",
        };

        open_draft(&request, &provider).await.unwrap();

        let log_output = std::process::Command::new("git")
            .current_dir(origin.path())
            .args(["log", "-1", "--format=%H", "refs/heads/warden/run-3"])
            .output()
            .unwrap();
        let origin_head = String::from_utf8_lossy(&log_output.stdout)
            .trim()
            .to_string();
        assert_eq!(
            origin_head, skeleton_sha,
            "origin must land on exactly the skeleton commit, never the branch tip \
             that happens to sit on top of it locally"
        );

        let ls_tree = std::process::Command::new("git")
            .current_dir(origin.path())
            .args(["ls-tree", "-r", "--name-only", "refs/heads/warden/run-3"])
            .output()
            .unwrap();
        let files = String::from_utf8_lossy(&ls_tree.stdout);
        assert!(
            files.contains("SKELETON.md"),
            "skeleton file missing: {files}"
        );
        assert!(
            !files.contains("src_business.rs"),
            "business code must never reach origin via open_draft: {files}"
        );
    }

    /// The security crux (issue #4 review, finding #1): `open_draft` must
    /// not blindly trust a caller-supplied sha to be a content-free
    /// skeleton. Here the caller (standing in for a buggy/compromised
    /// orchestrator) hands it the *business* commit instead of the
    /// skeleton -- `open_draft` must independently detect the extra file
    /// relative to `origin/main`, refuse, and push nothing at all.
    #[tokio::test]
    async fn open_draft_rejects_a_skeleton_sha_that_carries_business_content() {
        let (origin, gate_repo, _skeleton_sha, business_sha) =
            gate_repo_with_business_commit_on_top_of_skeleton();
        let provider = RecordingProvider::new(13);

        let request = OpenDraftRequest {
            bare_repo_path: gate_repo.path(),
            skeleton_commit_sha: &business_sha,
            branch: "warden/run-rejected",
            base_branch: "main",
            intent: "Add a feature",
        };

        let result = open_draft(&request, &provider).await;

        match result {
            Err(GatedError::SkeletonNotContentFree {
                commit_sha, files, ..
            }) => {
                assert_eq!(commit_sha, business_sha);
                assert_eq!(files, vec!["src_business.rs".to_string()]);
            }
            other => panic!("expected SkeletonNotContentFree, got {other:?}"),
        }

        assert!(
            provider.calls().is_empty(),
            "a rejected skeleton must never reach the PR provider"
        );

        let ref_check = std::process::Command::new("git")
            .current_dir(origin.path())
            .args(["rev-parse", "--verify", "refs/heads/warden/run-rejected"])
            .output()
            .unwrap();
        assert!(
            !ref_check.status.success(),
            "origin must never receive a push for a rejected skeleton"
        );
    }

    /// The range-check follow-up (issue #4 review): an *endpoint-only* diff
    /// (skeleton tip tree vs. base tip tree) would miss this entirely --
    /// the skeleton's tip tree is identical to `origin/main`'s, because an
    /// intermediate commit adds `secret.rs` and a later commit removes it
    /// again. But `git push {skeleton}:refs/heads/<branch>` still transfers
    /// every object in that range, so `secret.rs`'s blob would land on
    /// `origin` and stay reachable via `git log`/`git show` on the pushed
    /// branch even though the tree at the tip never shows it. `open_draft`
    /// must catch this by checking every commit in the range individually,
    /// not just the range's net effect.
    #[tokio::test]
    async fn open_draft_rejects_a_skeleton_whose_tip_matches_base_but_whose_history_leaks_a_secret_file(
    ) {
        let origin = tempfile::TempDir::new().unwrap();
        run_git(origin.path(), &["init", "--bare", "--quiet"]);

        let seed = tempfile::TempDir::new().unwrap();
        run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
        run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
        run_git(seed.path(), &["config", "user.name", "warden-test"]);
        std::fs::write(seed.path().join("f.txt"), "skeleton\n").unwrap();
        run_git(seed.path(), &["add", "."]);
        run_git(seed.path(), &["commit", "--quiet", "-m", "skeleton"]);
        run_git(
            seed.path(),
            &[
                "push",
                "--quiet",
                &origin.path().display().to_string(),
                "main",
            ],
        );

        // Intermediate commit: adds a secret file that must never reach
        // `origin`.
        std::fs::write(seed.path().join("secret.rs"), "const KEY: &str = \"x\";\n").unwrap();
        run_git(seed.path(), &["add", "."]);
        run_git(seed.path(), &["commit", "--quiet", "-m", "add secret.rs"]);

        // Later commit: removes it again, so the *tip* tree is byte-for-
        // byte identical to `main`'s -- an endpoint-only diff would see
        // nothing wrong here.
        run_git(seed.path(), &["rm", "--quiet", "secret.rs"]);
        run_git(
            seed.path(),
            &["commit", "--quiet", "-m", "remove secret.rs"],
        );
        let tip_sha = {
            let output = std::process::Command::new("git")
                .current_dir(seed.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };

        let gate_repo = tempfile::TempDir::new().unwrap();
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

        let provider = RecordingProvider::new(29);
        let request = OpenDraftRequest {
            bare_repo_path: gate_repo.path(),
            skeleton_commit_sha: &tip_sha,
            branch: "warden/run-leaky-history",
            base_branch: "main",
            intent: "Add a feature",
        };

        let result = open_draft(&request, &provider).await;
        match result {
            Err(GatedError::SkeletonNotContentFree {
                commit_sha, files, ..
            }) => {
                assert_eq!(commit_sha, tip_sha);
                assert_eq!(files, vec!["secret.rs".to_string()]);
            }
            other => panic!("expected SkeletonNotContentFree, got {other:?}"),
        }

        assert!(
            provider.calls().is_empty(),
            "a rejected skeleton must never reach the PR provider"
        );

        let ref_check = std::process::Command::new("git")
            .current_dir(origin.path())
            .args([
                "rev-parse",
                "--verify",
                "refs/heads/warden/run-leaky-history",
            ])
            .output()
            .unwrap();
        assert!(
            !ref_check.status.success(),
            "origin must never receive a push when the range leaks a secret file, \
             even if the tip tree matches base"
        );
    }

    /// Exercises the other content-free path (finding #1's "e.g. ... or the
    /// empty tree" case): when `base_branch` doesn't exist on `origin` yet
    /// (a brand-new repo, first run ever), a skeleton with a genuinely
    /// empty tree is still accepted.
    #[tokio::test]
    async fn open_draft_accepts_an_empty_tree_skeleton_when_base_branch_does_not_exist_on_origin() {
        let origin = tempfile::TempDir::new().unwrap();
        run_git(origin.path(), &["init", "--bare", "--quiet"]);

        let seed = tempfile::TempDir::new().unwrap();
        run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
        run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
        run_git(seed.path(), &["config", "user.name", "warden-test"]);
        run_git(
            seed.path(),
            &["commit", "--quiet", "--allow-empty", "-m", "skeleton"],
        );
        let skeleton_sha = {
            let output = std::process::Command::new("git")
                .current_dir(seed.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        // Deliberately never pushed to `origin` -- `origin` stays empty, so
        // `main` doesn't exist there yet.

        let gate_repo = tempfile::TempDir::new().unwrap();
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

        let provider = RecordingProvider::new(17);
        let request = OpenDraftRequest {
            bare_repo_path: gate_repo.path(),
            skeleton_commit_sha: &skeleton_sha,
            branch: "warden/run-first-ever",
            base_branch: "main",
            intent: "Bootstrap the repo",
        };

        let pr = open_draft(&request, &provider).await.unwrap();
        assert_eq!(pr.number, 17);

        let origin_head = std::process::Command::new("git")
            .current_dir(origin.path())
            .args([
                "log",
                "-1",
                "--format=%H",
                "refs/heads/warden/run-first-ever",
            ])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&origin_head.stdout).trim(),
            skeleton_sha
        );
    }

    /// The other half of the empty-tree fallback (finding #1): when
    /// `base_branch` doesn't exist on `origin` yet, the *only* content-free
    /// skeleton is one with a genuinely empty tree -- a skeleton that adds
    /// real files must still be rejected, exactly as it would be against an
    /// existing base branch. Without this, a brand-new repo's very first
    /// run would let *any* skeleton content through unchecked.
    #[tokio::test]
    async fn open_draft_rejects_a_non_empty_skeleton_when_base_branch_does_not_exist_on_origin() {
        let origin = tempfile::TempDir::new().unwrap();
        run_git(origin.path(), &["init", "--bare", "--quiet"]);

        let seed = tempfile::TempDir::new().unwrap();
        run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
        run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
        run_git(seed.path(), &["config", "user.name", "warden-test"]);
        std::fs::write(seed.path().join("src_business.rs"), "fn business() {}\n").unwrap();
        run_git(seed.path(), &["add", "."]);
        run_git(
            seed.path(),
            &["commit", "--quiet", "-m", "not actually a skeleton"],
        );
        let non_empty_sha = {
            let output = std::process::Command::new("git")
                .current_dir(seed.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        // Deliberately never pushed to `origin` -- `main` doesn't exist
        // there yet, same as the accept-case fixture.

        let gate_repo = tempfile::TempDir::new().unwrap();
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

        let provider = RecordingProvider::new(19);
        let request = OpenDraftRequest {
            bare_repo_path: gate_repo.path(),
            skeleton_commit_sha: &non_empty_sha,
            branch: "warden/run-first-ever-rejected",
            base_branch: "main",
            intent: "Bootstrap the repo",
        };

        let result = open_draft(&request, &provider).await;
        match result {
            Err(GatedError::SkeletonNotContentFree {
                commit_sha, files, ..
            }) => {
                assert_eq!(commit_sha, non_empty_sha);
                assert_eq!(files, vec!["src_business.rs".to_string()]);
            }
            other => panic!("expected SkeletonNotContentFree, got {other:?}"),
        }

        assert!(
            provider.calls().is_empty(),
            "a rejected first-run skeleton must never reach the PR provider"
        );
        let ref_check = std::process::Command::new("git")
            .current_dir(origin.path())
            .args([
                "rev-parse",
                "--verify",
                "refs/heads/warden/run-first-ever-rejected",
            ])
            .output()
            .unwrap();
        assert!(
            !ref_check.status.success(),
            "origin must never receive a push for a rejected first-run skeleton"
        );
    }

    /// Ordering invariant (issue #4 review, finding #2): pure validation
    /// (here, a blank intent that `generate_pr_title` refuses) must surface
    /// *before* `open_draft` does anything irreversible -- no content-free
    /// check, no push, no provider call. Proven here against a real gate
    /// repo/origin pair rather than just unit-testing `generate_pr_title` in
    /// isolation, so the ordering itself (not just the pure function) is
    /// under test.
    #[tokio::test]
    async fn open_draft_rejects_a_blank_intent_before_touching_git_or_the_provider() {
        let (origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
        let provider = RecordingProvider::new(23);

        let request = OpenDraftRequest {
            bare_repo_path: gate_repo.path(),
            skeleton_commit_sha: &commit_sha,
            branch: "warden/run-blank-intent",
            base_branch: "main",
            intent: "   \n  \n",
        };

        let result = open_draft(&request, &provider).await;
        assert!(matches!(result, Err(GatedError::EmptyIntent)));

        assert!(
            provider.calls().is_empty(),
            "a blank intent must be rejected before the PR provider is ever called"
        );
        let ref_check = std::process::Command::new("git")
            .current_dir(origin.path())
            .args([
                "rev-parse",
                "--verify",
                "refs/heads/warden/run-blank-intent",
            ])
            .output()
            .unwrap();
        assert!(
            !ref_check.status.success(),
            "a blank intent must be rejected before anything is pushed to origin"
        );
    }

    #[tokio::test]
    async fn post_cycle_update_only_ever_posts_a_comment() {
        let provider = RecordingProvider::new(1);
        let pr = PrHandle { number: 7 };
        let summary = CycleSummary {
            cycle_number: 2,
            findings: vec![reviewer_finding()],
        };

        post_cycle_update(&pr, &summary, &provider).await.unwrap();

        let calls = provider.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with("post_comment(7,"));
    }

    async fn seeded_run_db(
        state: warden_core::RunState,
        converged_commit_sha: Option<&str>,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let write_options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let write_pool = SqlitePoolOptions::new()
            .connect_with(write_options)
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE runs (id TEXT PRIMARY KEY, state TEXT NOT NULL, converged_commit_sha TEXT)",
        )
        .execute(&write_pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO runs (id, state, converged_commit_sha) VALUES ('run-1', ?, ?)")
            .bind(state.as_str())
            .bind(converged_commit_sha)
            .execute(&write_pool)
            .await
            .unwrap();
        write_pool.close().await;

        (dir, db_path)
    }

    #[tokio::test]
    async fn finalize_pushes_and_marks_ready_when_converged_and_hash_matches() {
        let (_origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
        let (_db_dir, db_path) =
            seeded_run_db(warden_core::RunState::Converged, Some(&commit_sha)).await;
        let pool = crate::db::connect_read_only(&db_path).await.unwrap();
        let provider = RecordingProvider::new(1);
        let pr = PrHandle { number: 7 };

        let request = FinalizeRequest {
            bare_repo_path: gate_repo.path(),
            branch: "main",
            run_id: "run-1",
            pushed_commit_sha: &commit_sha,
            pr: &pr,
            summary_body: "full run summary",
        };
        let outcome = finalize(&pool, &request, &provider).await.unwrap();

        assert_eq!(
            outcome,
            FinalizeOutcome::Finalized {
                commit_sha: commit_sha.clone()
            }
        );

        let calls = provider.calls();
        assert_eq!(
            calls,
            vec![
                "update_body(7, full run summary)".to_string(),
                "mark_ready(7)".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn finalize_blocks_and_never_touches_the_provider_when_not_converged() {
        let (_origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
        let (_db_dir, db_path) = seeded_run_db(warden_core::RunState::CoderRunning, None).await;
        let pool = crate::db::connect_read_only(&db_path).await.unwrap();
        let provider = RecordingProvider::new(1);
        let pr = PrHandle { number: 7 };

        let request = FinalizeRequest {
            bare_repo_path: gate_repo.path(),
            branch: "main",
            run_id: "run-1",
            pushed_commit_sha: &commit_sha,
            pr: &pr,
            summary_body: "full run summary",
        };
        let outcome = finalize(&pool, &request, &provider).await.unwrap();

        assert_eq!(
            outcome,
            FinalizeOutcome::Blocked(GateBlockReason::NotConverged {
                actual_state: warden_core::RunState::CoderRunning
            })
        );
        assert!(
            provider.calls().is_empty(),
            "a blocked finalize must never call the provider"
        );
    }

    #[tokio::test]
    async fn finalize_blocks_and_never_touches_the_provider_on_hash_mismatch() {
        let (origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
        let (_db_dir, db_path) = seeded_run_db(
            warden_core::RunState::Converged,
            Some("some-other-validated-sha"),
        )
        .await;
        let pool = crate::db::connect_read_only(&db_path).await.unwrap();
        let provider = RecordingProvider::new(1);
        let pr = PrHandle { number: 7 };

        let request = FinalizeRequest {
            bare_repo_path: gate_repo.path(),
            branch: "main",
            run_id: "run-1",
            pushed_commit_sha: &commit_sha,
            pr: &pr,
            summary_body: "full run summary",
        };
        let outcome = finalize(&pool, &request, &provider).await.unwrap();

        assert_eq!(
            outcome,
            FinalizeOutcome::Blocked(GateBlockReason::HashMismatch {
                validated: Some("some-other-validated-sha".to_string()),
                pushed: commit_sha.clone(),
            })
        );
        assert!(
            provider.calls().is_empty(),
            "a blocked finalize (hash mismatch) must never call the provider"
        );

        // `gate_repo_fixture` seeds `origin`'s `main` with `commit_sha` up
        // front (so `open_draft`'s content-free check has a real base to
        // diff against elsewhere) -- a blocked finalize must leave that ref
        // exactly as it already was, not add or move anything.
        let origin_head = std::process::Command::new("git")
            .current_dir(origin.path())
            .args(["log", "-1", "--format=%H", "refs/heads/main"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&origin_head.stdout).trim(),
            commit_sha,
            "a blocked finalize must never push anything to origin"
        );
    }

    /// Tracks a fake PR's actual `draft`/`body` state (not just a call log)
    /// -- lets this test assert the invariant itself ("the PR's draft status
    /// flips only at `Finalize`, never at `PostCycleUpdate`") rather than
    /// only that the right provider method got called.
    struct StatefulProvider {
        next_pr_number: u64,
        draft: std::sync::Mutex<bool>,
        body: std::sync::Mutex<String>,
    }

    impl StatefulProvider {
        fn new(next_pr_number: u64) -> Self {
            Self {
                next_pr_number,
                draft: std::sync::Mutex::new(true),
                body: std::sync::Mutex::new(String::new()),
            }
        }

        fn is_draft(&self) -> bool {
            *self.draft.lock().unwrap()
        }

        fn body(&self) -> String {
            self.body.lock().unwrap().clone()
        }
    }

    impl PrProvider for StatefulProvider {
        async fn open_draft(&self, params: &OpenDraftParams<'_>) -> Result<PrHandle> {
            *self.body.lock().unwrap() = params.body.to_string();
            Ok(PrHandle {
                number: self.next_pr_number,
            })
        }

        async fn post_comment(&self, _pr: &PrHandle, _body: &str) -> Result<()> {
            Ok(())
        }

        async fn mark_ready(&self, _pr: &PrHandle) -> Result<()> {
            *self.draft.lock().unwrap() = false;
            Ok(())
        }

        async fn update_body(&self, _pr: &PrHandle, body: &str) -> Result<()> {
            *self.body.lock().unwrap() = body.to_string();
            Ok(())
        }
    }

    /// End-to-end across the full lifecycle a real run drives
    /// `pr_manager` through: `OpenDraft`, two `PostCycleUpdate`s, then
    /// `Finalize`. Asserts the ticket's core orchestration invariant --
    /// the PR never leaves draft, and its body never changes, until
    /// `Finalize` explicitly does both -- against a provider double that
    /// tracks real state rather than just recording calls.
    #[tokio::test]
    async fn pr_never_leaves_draft_or_changes_body_before_finalize_across_a_full_cycle_sequence() {
        let (_origin, _seed, gate_repo, skeleton_sha) = gate_repo_fixture();
        let provider = StatefulProvider::new(21);

        let open_request = OpenDraftRequest {
            bare_repo_path: gate_repo.path(),
            skeleton_commit_sha: &skeleton_sha,
            branch: "warden/run-4",
            base_branch: "main",
            intent: "Add JWT expiry handling",
        };
        let pr = open_draft(&open_request, &provider).await.unwrap();
        assert!(
            provider.is_draft(),
            "PR must be a draft immediately after OpenDraft"
        );
        let body_after_open = provider.body();

        for cycle in 1..=2 {
            let summary = CycleSummary {
                cycle_number: cycle,
                findings: vec![reviewer_finding()],
            };
            post_cycle_update(&pr, &summary, &provider).await.unwrap();
            assert!(
                provider.is_draft(),
                "PostCycleUpdate must never flip a PR out of draft (cycle {cycle})"
            );
            assert_eq!(
                provider.body(),
                body_after_open,
                "PostCycleUpdate must never touch the PR body (cycle {cycle})"
            );
        }

        let (_db_dir, db_path) =
            seeded_run_db(warden_core::RunState::Converged, Some(&skeleton_sha)).await;
        let pool = crate::db::connect_read_only(&db_path).await.unwrap();
        let finalize_request = FinalizeRequest {
            bare_repo_path: gate_repo.path(),
            branch: "warden/run-4",
            run_id: "run-1",
            pushed_commit_sha: &skeleton_sha,
            pr: &pr,
            summary_body: "full run summary",
        };
        let outcome = finalize(&pool, &finalize_request, &provider).await.unwrap();

        assert_eq!(
            outcome,
            FinalizeOutcome::Finalized {
                commit_sha: skeleton_sha.clone()
            }
        );
        assert!(
            !provider.is_draft(),
            "Finalize must be the point where the PR leaves draft"
        );
        assert_eq!(
            provider.body(),
            "full run summary",
            "Finalize must update the PR body to the full run summary"
        );
    }
}
