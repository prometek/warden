use super::*;

/// Detects `fixes #123` / `closes #123` / `resolves #123` (any case) inside a run's intent, per
/// ("liée à l'issue détectée dans l'intent").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedIssue {
    pub number: u64,
    /// The exact keyword as written in the intent (case preserved).
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
pub fn detect_linked_issue(intent: &str) -> Option<LinkedIssue> {
    let captures = linked_issue_pattern().captures(intent)?;
    let keyword = captures.get(1)?.as_str().to_string();
    let number = captures.get(2)?.as_str().parse().ok()?;
    Some(LinkedIssue { number, keyword })
}

pub(super) const MAX_GENERATED_TITLE_LEN: usize = 72;

/// Generates a PR title from an intent when nothing more specific is available: the intent's first
/// non-blank line, truncated to a sane length.
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

/// Builds the draft PR body: the linked-issue reference (if any, so GitHub auto-links/auto-closes
/// it on merge), the intent verbatim, and a fixed note marking this as a skeleton draft.
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

/// Role of the commit's author agent, embedded in the `Warden-Agent` trailer.
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

/// The three structured commit trailers coder/doc commits carry locally (Architecture.md §5.3) --
/// no remote access needed to produce these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitTrailers {
    pub cycle: u32,
    pub findings_resolved: Vec<String>,
    pub agent: TrailerAgent,
}

impl CommitTrailers {
    /// Renders just the trailer block, one `Key: value` line per trailer, in the order shown in
    /// Architecture.md §5.3's table.
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

/// Appends `trailers` to `commit_message`, separated by the blank line git trailers require.
pub fn append_trailers(commit_message: &str, trailers: &CommitTrailers) -> String {
    format!("{}\n\n{}\n", commit_message.trim_end(), trailers.format())
}

#[derive(Debug, Clone)]
pub struct CycleSummary {
    pub cycle_number: u32,
    pub findings: Vec<Finding>,
}

/// Renders one cycle's findings into the PR comment body `post_cycle_update` posts.
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
