use super::*;

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

pub(super) const MAX_GENERATED_TITLE_LEN: usize = 72;

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
///
/// Issue #24 review, cycle 2, MINOR: every [`FindingSource`] variant is
/// listed explicitly here, not just `Reviewer`/`Tester` -- the original
/// two-source grouping silently rendered a **blank** comment (just the
/// header and the trailing "informational only" line, no findings section
/// at all) for a cycle whose only findings came from a source outside that
/// list, e.g. `FindingSource::Warden` (issue #24 review M4, the
/// `.warden/agents/` tampering check) once `post_cycle_update` gains a
/// production caller. Latent today (nothing calls `post_cycle_update` in
/// production yet -- `warden`'s own `pr_summary::format_cycles_section`
/// renders the Finalize-time PR body and never filtered by source at all),
/// but there is no reason a *new* `FindingSource` variant should ever have
/// to remember to add itself here to avoid silently vanishing.
pub fn format_cycle_comment(summary: &CycleSummary) -> String {
    let mut body = format!("## Warden — cycle {} update\n\n", summary.cycle_number);

    if summary.findings.is_empty() {
        body.push_str("No findings raised this cycle.\n\n");
    } else {
        for source in [
            FindingSource::role("reviewer"),
            FindingSource::role("tester"),
            FindingSource::Warden,
            FindingSource::Ci,
        ] {
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
