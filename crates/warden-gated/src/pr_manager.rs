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
    /// The branch-skeleton commit -- **must** contain no business code; that
    /// invariant is the caller's responsibility (the orchestrator only ever
    /// hands the gate a skeleton commit before the coder has run).
    pub skeleton_commit_sha: &'a str,
    pub branch: &'a str,
    pub base_branch: &'a str,
    pub intent: &'a str,
}

/// `OpenDraft` (ADR-0007): pushes only the branch skeleton to `origin`, then
/// opens a draft PR linked to the issue the intent references (or titled
/// from the intent otherwise). Triggered at coder start -- before any
/// business code exists, so this is the earliest point metadata is allowed
/// to reach `origin` under ADR-0002/0007.
pub async fn open_draft<P: PrProvider>(
    request: &OpenDraftRequest<'_>,
    provider: &P,
) -> Result<PrHandle> {
    push::push_to_origin(
        request.bare_repo_path,
        request.skeleton_commit_sha,
        request.branch,
    )
    .await?;

    let linked_issue = detect_linked_issue(request.intent);
    let title = generate_pr_title(request.intent)?;
    let body = open_draft_pr_body(request.intent, linked_issue.as_ref());

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
            self.calls
                .lock()
                .unwrap()
                .push(format!("open_draft({}, {})", params.branch, params.title));
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

    /// A bare gate repo with `origin` pointed at a second local bare repo,
    /// plus the sha of a commit already present in the gate repo -- enough
    /// to exercise `push::push_to_origin` (which `open_draft`/`finalize`
    /// both call) without any network access.
    fn gate_repo_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        tempfile::TempDir,
        String,
    ) {
        let origin = tempfile::TempDir::new().unwrap();
        run_git(origin.path(), &["init", "--bare", "--quiet"]);

        let seed = tempfile::TempDir::new().unwrap();
        run_git(seed.path(), &["init", "--quiet"]);
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
}
