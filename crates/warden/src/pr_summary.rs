//! Composes the PR body handed to `warden-gated::pr_manager::finalize` as
//! `summary_body` (ADR-0007 Finalize) once a run converges, including the
//! Evidence section ADR-0009 requires. Pure formatting only -- the caller
//! (the orchestrator, via `warden::db`) has already read every
//! run/cycle/finding/evidence row this needs; this module never touches the
//! database or the filesystem itself.
//!
//! Deliberately not shared with `warden_gated::pr_manager` (which formats
//! `PostCycleUpdate`'s per-cycle comment): `warden` is the only crate with
//! full read access to a run's cycles/findings/evidence history (ADR-0006 --
//! `warden-gated` only ever re-reads the minimal state/converged-commit view
//! it needs to authorize a push), so the full summary is assembled here and
//! handed to `warden-gated` as an opaque string it just posts.

use warden_core::{EvidenceType, Finding, RunState};

/// One cycle's findings, as already parsed/persisted by the orchestrator --
/// this module only formats them, it never re-derives or re-validates
/// finding content (mirrors `warden_gated::pr_manager::CycleSummary`, kept
/// as a separate type since `warden` must not depend on `warden-gated`,
/// ADR-0006).
#[derive(Debug, Clone)]
pub struct CycleSummary {
    pub cycle_number: u32,
    pub findings: Vec<Finding>,
}

/// One piece of evidence captured during the run, already committed into
/// the repo (`evidence::commit_evidence_into_repo`) by the time this is
/// rendered -- `repo_relative_path` is where it now lives.
#[derive(Debug, Clone)]
pub struct EvidenceSummary {
    pub cycle_number: u32,
    pub evidence_type: EvidenceType,
    pub repo_relative_path: String,
    pub description: String,
}

/// Everything [`pr_body_from_run`] needs about the run itself, gathered by
/// the caller from `warden::db`.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub run_id: String,
    pub intent: String,
    pub final_state: RunState,
    pub cycles: Vec<CycleSummary>,
}

/// Builds the full PR body `Finalize` (ADR-0007) replaces the draft body
/// with: the intent, a per-cycle findings recap, and -- when any evidence
/// was captured -- an Evidence section (ADR-0009): images embedded inline
/// via the branch's raw-content URL, everything else (video, log,
/// asciinema recordings) as a clickable link, since markdown can't embed
/// those inline.
///
/// `repo_slug` is `"<owner>/<repo>"` and `branch` the branch the evidence
/// was pushed on -- both needed to build the
/// `raw.githubusercontent.com/<repo_slug>/<branch>/<path>` URLs ADR-0009
/// specifies.
pub fn pr_body_from_run(
    summary: &RunSummary,
    evidence: &[EvidenceSummary],
    repo_slug: &str,
    branch: &str,
) -> String {
    let mut sections = vec![
        summary.intent.trim().to_string(),
        format_cycles_section(summary),
    ];
    if !evidence.is_empty() {
        sections.push(format_evidence_section(evidence, repo_slug, branch));
    }
    sections.push(format!(
        "---\n_Finalized by Warden after {} cycle(s) — run `{}` converged to `{:?}`._",
        summary.cycles.len(),
        summary.run_id,
        summary.final_state,
    ));
    sections.join("\n\n")
}

fn format_cycles_section(summary: &RunSummary) -> String {
    let mut body = "## Cycles\n\n".to_string();
    if summary.cycles.is_empty() {
        body.push_str("No cycles were recorded for this run.\n");
        return body;
    }
    for cycle in &summary.cycles {
        body.push_str(&format!("**Cycle {}**\n\n", cycle.cycle_number));
        if cycle.findings.is_empty() {
            body.push_str("No findings raised.\n\n");
            continue;
        }
        for finding in &cycle.findings {
            body.push_str(&format_finding_line(finding));
            body.push('\n');
        }
        body.push('\n');
    }
    body
}

fn format_finding_line(finding: &Finding) -> String {
    let location = finding
        .file
        .as_deref()
        .map(|file| format!(" ({file})"))
        .unwrap_or_default();
    format!(
        "- [{}/{}]{location} {}",
        finding.source.as_str(),
        finding.severity.as_str(),
        finding.description
    )
}

/// Renders the Evidence section (ADR-0009): images inline via the branch's
/// `raw.githubusercontent.com` URL, everything else (video/log/asciinema
/// recordings, the latter always typed [`EvidenceType::Other`]) as a
/// clickable link -- GitHub's markdown renderer can't embed a video or a
/// `.cast` file inline the way it can a `<picture>`-backed image.
fn format_evidence_section(evidence: &[EvidenceSummary], repo_slug: &str, branch: &str) -> String {
    let mut body = "## Evidence\n\n".to_string();
    for item in evidence {
        let raw_url = format!(
            "https://raw.githubusercontent.com/{repo_slug}/{branch}/{}",
            item.repo_relative_path
        );
        match item.evidence_type {
            EvidenceType::Image => {
                body.push_str(&format!(
                    "**Cycle {}** — ![{}]({raw_url})\n\n",
                    item.cycle_number, item.description
                ));
            }
            EvidenceType::Video | EvidenceType::Log | EvidenceType::Other => {
                body.push_str(&format!(
                    "**Cycle {}** — [{}]({raw_url})\n\n",
                    item.cycle_number, item.description
                ));
            }
        }
    }
    body
}
