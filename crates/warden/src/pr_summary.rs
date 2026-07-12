//! Composes the PR body handed to `warden-gated::pr_manager::finalize` as
//! `summary_body` (ADR-0007 Finalize) once a run converges, including the
//! Evidence section ADR-0009 requires. Pure formatting only -- the caller
//! (the orchestrator, via `warden::db`) has already read every
//! run/cycle/finding/evidence row this needs; this module never touches the
//! database or the filesystem itself.
//!
//! The cycles/findings recap is deliberately not shared with
//! `warden_gated::pr_manager` (which only formats `PostCycleUpdate`'s
//! per-cycle comment): `warden` is the only crate with full read access to a
//! run's cycles/findings history (ADR-0006 -- `warden-gated` only ever
//! re-reads the minimal state/converged-commit view it needs to authorize a
//! push), so that part of the summary is assembled here.
//!
//! The Evidence section itself, however, is rendered via
//! `warden_core::format_evidence_section` rather than a local copy: it lives
//! in `warden-core` precisely so `warden-gated`'s own `finalize` (the only
//! code that actually sets a PR body in production, see
//! `pr_manager::finalize_pr_body`) can render it too, independently of
//! whatever `summary_body` this module hands over the IPC boundary --
//! before this, the evidence renderer lived only in this crate, which
//! `warden-gated` can never depend on (ADR-0006), making it structurally
//! dead code (see `pr_manager::FinalizeRequest`'s docs for the residual
//! seam issue #4 still needs to connect).

use warden_core::{EvidenceRow, Finding, RunState};

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
    evidence: &[EvidenceRow],
    repo_slug: &str,
    branch: &str,
) -> String {
    let mut sections = vec![
        summary.intent.trim().to_string(),
        format_cycles_section(summary),
    ];
    if !evidence.is_empty() {
        sections.push(warden_core::format_evidence_section(
            evidence, repo_slug, branch,
        ));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run_summary() -> RunSummary {
        RunSummary {
            run_id: "run-1".to_string(),
            intent: "make the button clickable".to_string(),
            final_state: RunState::Converged,
            cycles: vec![CycleSummary {
                cycle_number: 1,
                findings: vec![],
            }],
        }
    }

    // -----------------------------------------------------------------
    // Acceptance criterion 6: the Evidence section is absent when there
    // is no evidence. The per-row rendering (inline images vs. clickable
    // links, percent-encoding, ...) is tested where `format_evidence_section`
    // now lives: `warden_core::pr_body`.
    // -----------------------------------------------------------------

    #[test]
    fn pr_body_omits_the_evidence_section_when_no_evidence_was_captured() {
        let body = pr_body_from_run(&run_summary(), &[], "owner/repo", "main");
        assert!(
            !body.contains("## Evidence"),
            "PR body must not contain an Evidence section when nothing was captured: {body}"
        );
    }

    #[test]
    fn pr_body_includes_the_evidence_section_when_evidence_is_present() {
        let evidence = vec![EvidenceRow {
            cycle_number: 1,
            evidence_type: warden_core::EvidenceType::Image,
            repo_relative_path: ".warden/evidence/1/screenshot.png".to_string(),
            description: "login screen".to_string(),
        }];
        let body = pr_body_from_run(&run_summary(), &evidence, "owner/repo", "main");
        assert!(body.contains("## Evidence"), "body was: {body}");
    }

    #[test]
    fn pr_body_delegates_evidence_rendering_to_warden_core_format_evidence_section() {
        // Guards against the HIGH finding regressing: this must produce the
        // exact same markdown `warden_core::format_evidence_section` does,
        // proving `pr_body_from_run` doesn't carry its own drifted copy.
        let evidence = vec![EvidenceRow {
            cycle_number: 1,
            evidence_type: warden_core::EvidenceType::Image,
            repo_relative_path: ".warden/evidence/1/screenshot.png".to_string(),
            description: "login screen".to_string(),
        }];
        let body = pr_body_from_run(&run_summary(), &evidence, "acme/widgets", "main");
        let expected_section =
            warden_core::format_evidence_section(&evidence, "acme/widgets", "main");
        assert!(
            body.contains(expected_section.trim_end()),
            "body was: {body}"
        );
    }

    #[test]
    fn pr_body_places_the_evidence_section_after_the_cycles_section() {
        // Composition-level ordering that belongs here, not in
        // `warden_core::pr_body` (which only renders the section in
        // isolation, with no notion of "cycles section" at all).
        let evidence = vec![EvidenceRow {
            cycle_number: 1,
            evidence_type: warden_core::EvidenceType::Log,
            repo_relative_path: ".warden/evidence/1/run.log".to_string(),
            description: "test run log".to_string(),
        }];
        let body = pr_body_from_run(&run_summary(), &evidence, "acme/widgets", "main");

        let cycles_pos = body.find("## Cycles").expect("body was: {body}");
        let evidence_pos = body.find("## Evidence").expect("body was: {body}");
        assert!(
            cycles_pos < evidence_pos,
            "the Cycles section must come before the Evidence section: {body}"
        );
    }
}
