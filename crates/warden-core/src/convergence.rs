//! Convergence rules: interpreting findings from reviewer/tester agents and
//! deciding the next [`RunState`]. Pure logic — no I/O, no clock, no
//! subprocess. Parsing of agent stdout also lives here since it's the
//! boundary where untrusted external input is validated before it can ever
//! reach the state machine (code-standards.md, "Validation à la frontière").

use serde::Deserialize;

use crate::error::{CoreError, Result};
use crate::state::{AgentRole, RunState};

/// Which agent (or, for CI/Warden itself, which non-agent process) raised a
/// finding (`FINDINGS.source`). `Ci` (issue #5) covers a failing check
/// surfaced by `warden-gated`'s CI watcher; `Warden` (issue #24 review, M4)
/// covers a finding the orchestrator raises directly from a structural check
/// against the coder's own diff (currently: a cycle's coder commit touching
/// `.warden/agents/`, `warden::orchestrator::agent_definition_tampering_finding`)
/// -- both are distinct from `Reviewer`/`Tester` since neither ever comes
/// from an agent subprocess's own judgement at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingSource {
    Reviewer,
    Tester,
    Ci,
    Warden,
}

impl FindingSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingSource::Reviewer => "reviewer",
            FindingSource::Tester => "tester",
            FindingSource::Ci => "ci",
            FindingSource::Warden => "warden",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "reviewer" => Ok(FindingSource::Reviewer),
            "tester" => Ok(FindingSource::Tester),
            "ci" => Ok(FindingSource::Ci),
            "warden" => Ok(FindingSource::Warden),
            other => Err(CoreError::UnknownFindingSource(other.to_string())),
        }
    }
}

/// Severity of a finding (`FINDINGS.severity`). Only `Blocking` prevents
/// convergence; `Warning`/`Info` are recorded but never trigger a reboucle
/// on their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Blocking,
    Warning,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Blocking => "blocking",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "blocking" => Ok(Severity::Blocking),
            "warning" => Ok(Severity::Warning),
            "info" => Ok(Severity::Info),
            other => Err(CoreError::UnknownSeverity(other.to_string())),
        }
    }
}

/// A single finding raised by a reviewer or tester agent during a cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub source: FindingSource,
    pub severity: Severity,
    pub file: Option<String>,
    pub description: String,
    pub action: Option<String>,
}

/// Wire schema for a single line of the NDJSON findings stream (see
/// `parse_findings`): one finding per line, no wrapping object/array. Field
/// names/values are attacker-controlled (agent output is untrusted,
/// code-standards.md "Agent Subprocess Protocol") so every value is
/// validated against a closed set here, never passed through as a
/// free-form string.
#[derive(Debug, Deserialize)]
struct RawFinding {
    source: String,
    severity: String,
    file: Option<String>,
    description: String,
    action: Option<String>,
}

/// Parses an agent's stdout into a validated list of [`Finding`]s.
///
/// Wire format is **line-delimited JSON (NDJSON)**, per code-standards.md
/// "Agent Subprocess Protocol": "Chaque ligne stdout est une valeur JSON
/// validée (parse + schéma) avant d'atteindre la state machine" — one
/// finding object per non-blank line, not a single JSON blob for the whole
/// output. Blank lines are ignored.
///
/// Any non-blank line that isn't parsable JSON, or whose `severity`/
/// `source` isn't a known value, makes the whole call a
/// [`CoreError::MalformedAgentOutput`] — never a panic. We deliberately
/// don't try to salvage the lines that *did* parse: once a stream has shown
/// itself to produce output that doesn't match the protocol, treating the
/// rest of it as trustworthy would contradict code-standards.md "Ne jamais
/// faire confiance à la sortie d'un agent CLI". The caller (the
/// orchestrator) turns a parse failure into a blocking finding of its own,
/// not a crash of the run.
pub fn parse_findings(agent_stdout: &str) -> Result<Vec<Finding>> {
    agent_stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let raw: RawFinding = serde_json::from_str(line).map_err(|e| {
                CoreError::MalformedAgentOutput(format!("invalid JSON line {line:?}: {e}"))
            })?;
            Ok(Finding {
                source: FindingSource::parse(&raw.source)?,
                severity: Severity::parse(&raw.severity)?,
                file: raw.file,
                description: raw.description,
                action: raw.action,
            })
        })
        .collect()
}

/// Rejects any finding whose `source` isn't the one `role` is entitled to
/// claim -- `parse_findings` only validates that `source` is *some* known
/// value (issue #24 review, cycle 2, MAJOR 2), not that it's the value the
/// role that actually produced this stdout is allowed to use. Without this,
/// a reviewer/tester agent (whose raw output is untrusted input,
/// code-standards.md "Ne jamais faire confiance à la sortie d'un agent CLI")
/// could forge `source: "warden"` -- impersonating the structural finding
/// only Warden itself may raise
/// (`warden::orchestrator::agent_definition_tampering_finding`, issue #24
/// review M4) -- or `source: "ci"`, or its sibling role's own source. That
/// last one is the sharper, non-hypothetical case: a tester could mask a
/// real failure by emitting it as `source: "reviewer"` instead of
/// `"tester"`, since `warden::orchestrator::tester_succeeded` (the signal
/// that gates evidence capture) keys off `FindingSource::Tester` findings
/// specifically -- reviewer-mislabelled output would sail straight past it.
///
/// A reviewer may only ever emit [`FindingSource::Reviewer`]; a tester only
/// [`FindingSource::Tester`]. [`FindingSource::Warden`]/[`FindingSource::Ci`]
/// may never legitimately arrive from an agent subprocess's own stdout at
/// all -- both are raised by Warden's own code, never parsed from untrusted
/// agent output (`Ci` crosses a different, non-agent channel entirely,
/// `crate::ci_channel`). `role` is only ever `Reviewer`/`Tester` at this
/// validator's one call site (`warden::orchestrator::run_finding_agent`);
/// `AgentRole::Coder` is accepted defensively and always rejected, since the
/// coder role produces commits, never findings of its own.
///
/// Rejects the *whole* batch on the first mismatch found (index order),
/// rather than silently dropping just the offending line or silently
/// relabelling it to the expected source -- code-standards.md: "Valider
/// toute entrée externe à la frontière, avant qu'elle n'atteigne la state
/// machine", and once a stream has shown itself willing to misrepresent
/// even one finding's own origin, none of the rest of that same stream's
/// claims are trustworthy either (the same "don't salvage the parts that
/// looked fine" stance [`parse_findings`] already takes for a shape error).
pub fn validate_finding_sources_for_role(findings: &[Finding], role: AgentRole) -> Result<()> {
    let expected = match role {
        AgentRole::Reviewer => FindingSource::Reviewer,
        AgentRole::Tester => FindingSource::Tester,
        AgentRole::Coder => {
            return Err(CoreError::MalformedAgentOutput(
                "the coder role never raises findings of its own".to_string(),
            ));
        }
    };
    for (index, finding) in findings.iter().enumerate() {
        if finding.source != expected {
            return Err(CoreError::MalformedAgentOutput(format!(
                "finding at index {index} claims source {:?}, but the {role:?} role may only \
                 raise findings with source {:?}",
                finding.source.as_str(),
                expected.as_str(),
            )));
        }
    }
    Ok(())
}

/// Decides the next [`RunState`] once a cycle's findings are known -- called
/// from either [`RunState::Reviewing`] or [`RunState::Testing`], never
/// meaningful elsewhere (crash recovery, `MaxReviewCyclesExceeded`/
/// `MaxTestCyclesExceeded` -> `Failed`, ... do not go through this function).
///
/// **Issue #43 / ADR-0014, decision #37 Q1**: review and test now carry
/// *separate* cycle budgets, and a blocking finding is charged to whichever
/// budget its own phase belongs to -- never inferred from which `RunState`
/// the caller happened to be in, but from the finding's own
/// [`FindingSource`]:
///
/// - A blocking [`FindingSource::Reviewer`] or [`FindingSource::Warden`]
///   (the agent-definition-tampering check,
///   `warden::orchestrator::agent_definition_tampering_finding`) finding is a
///   **review-phase** failure -- checked against `max_review_cycles`. This is
///   what makes a *scoped* re-review (issue #41/#42: every re-review after a
///   tester finding's correctif is scoped, `ReviewScope::Correctif`) count
///   against the review budget rather than the test budget, exactly as
///   decision #37 Q1 requires -- it's still a `Reviewer`-sourced finding, so
///   it takes this branch regardless of what triggered the coder's correctif
///   in the first place.
/// - A blocking [`FindingSource::Tester`] finding is a **test-phase**
///   failure -- checked against `max_test_cycles`. By the caller's own
///   invariant (`warden::orchestrator::run_convergence_loop`: the tester
///   never runs on a cycle whose review wasn't clean, issue #41), `findings`
///   never carries both a blocking reviewer/tampering finding *and* a
///   blocking tester finding for the same call -- review-phase failures take
///   priority in the (unreachable in practice) case they did.
/// - No blocking finding at all: `Converged`.
///
/// `review_cycle`/`test_cycle` are the 1-based counts of review/test
/// invocations made so far *including this one* (the reviewer runs every
/// cycle, so `review_cycle` tracks the run's overall cycle number; the tester
/// only runs on review-clean cycles, so `test_cycle` only advances then) --
/// at or past the matching budget, the run exhausts that phase's budget
/// instead of reboucling.
pub fn decide_next_state(
    findings: &[Finding],
    review_cycle: u32,
    max_review_cycles: u32,
    test_cycle: u32,
    max_test_cycles: u32,
) -> RunState {
    let review_blocking = findings.iter().any(|f| {
        f.severity == Severity::Blocking
            && matches!(f.source, FindingSource::Reviewer | FindingSource::Warden)
    });
    let test_blocking = findings
        .iter()
        .any(|f| f.severity == Severity::Blocking && f.source == FindingSource::Tester);

    if !review_blocking && !test_blocking {
        return RunState::Converged;
    }

    if review_blocking {
        return if review_cycle >= max_review_cycles {
            RunState::MaxReviewCyclesExceeded
        } else {
            RunState::CoderRunning
        };
    }

    if test_cycle >= max_test_cycles {
        RunState::MaxTestCyclesExceeded
    } else {
        RunState::CoderRunning
    }
}

/// Coarse result of `warden-gated`'s CI watcher polling loop (issue #5),
/// passed in by the caller rather than re-derived here -- this module only
/// ever decides which [`RunState`] a given outcome implies, exactly like
/// [`decide_next_state`] does for reviewer/tester findings. Deliberately not
/// the same type as `warden-gated::ci_watcher::WatchOutcome`: that one
/// carries the full per-check `Finding` list for human-readable reporting,
/// while this crate only needs the coarse signal to pick a `RunState`
/// (`warden-gated` must never depend on `warden`, and `warden-core` must
/// never depend on `warden-gated` -- ADR-0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiOutcome {
    Merged,
    ChecksPassed,
    Closed,
    ChecksFailed,
    TimedOut,
}

/// Decides the next [`RunState`] once a run's CI watch (issue #5) reaches a
/// terminal outcome. Only meaningful from [`RunState::AwaitingCi`], whose
/// legal next states are exactly `Done` / `CoderRunning` / `Failed`
/// ([`RunState::validate_transition`]) -- notably neither
/// `MaxReviewCyclesExceeded` nor `MaxTestCyclesExceeded`, unlike
/// [`decide_next_state`]'s per-phase budget cases: a CI failure that exhausts
/// the cycle budget lands on `Failed` here instead. The caller
/// (`warden::orchestrator`) checks this against the review budget -- a CI
/// reboucle re-enters the loop at `CoderRunning` -> `Reviewing`, exactly like
/// any other reboucle to the coder.
///
/// - `Merged` / `ChecksPassed`: `Done`. Warden's own responsibility ends
///   once CI is green -- actually merging the PR is deliberately never
///   automatic (issue #5: "aucun merge automatique n'est déclenché par
///   Warden"), so `ChecksPassed` reaches the same terminal `RunState` as an
///   already-`Merged` PR; the merge itself is left entirely to a human.
/// - `Closed` (closed without merging) / `TimedOut`: `Failed` -- nothing
///   further for Warden to do, and neither represents a working result.
/// - `ChecksFailed`, with cycles remaining: `CoderRunning` (reboucle vers le
///   coder). At the cycle budget: `Failed`.
pub fn decide_next_state_after_ci(
    outcome: CiOutcome,
    current_cycle: u32,
    max_cycles: u32,
) -> RunState {
    match outcome {
        CiOutcome::Merged | CiOutcome::ChecksPassed => RunState::Done,
        CiOutcome::Closed | CiOutcome::TimedOut => RunState::Failed,
        CiOutcome::ChecksFailed => {
            if current_cycle >= max_cycles {
                RunState::Failed
            } else {
                RunState::CoderRunning
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocking_finding() -> Finding {
        Finding {
            source: FindingSource::Reviewer,
            severity: Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "unchecked unwrap".to_string(),
            action: Some("use ? instead".to_string()),
        }
    }

    /// Issue #24 review M4: the tampering finding
    /// (`warden::orchestrator::agent_definition_tampering_finding`) is
    /// `FindingSource::Warden`, folded in alongside the reviewer's own
    /// findings -- must be charged to the review budget exactly like a real
    /// reviewer finding (decision #37 Q1's imputation rule applies to it
    /// too, not only to `Reviewer`-sourced findings).
    fn tampering_finding() -> Finding {
        Finding {
            source: FindingSource::Warden,
            severity: Severity::Blocking,
            file: Some(".warden/agents/coder.md".to_string()),
            description: "agent definition tampering".to_string(),
            action: None,
        }
    }

    fn tester_blocking_finding() -> Finding {
        Finding {
            source: FindingSource::Tester,
            severity: Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "test fails".to_string(),
            action: Some("fix the panic".to_string()),
        }
    }

    fn info_finding() -> Finding {
        Finding {
            source: FindingSource::Tester,
            severity: Severity::Info,
            file: None,
            description: "consider adding a doc comment".to_string(),
            action: None,
        }
    }

    #[test]
    fn no_findings_converges() {
        assert_eq!(decide_next_state(&[], 1, 5, 0, 5), RunState::Converged);
    }

    #[test]
    fn only_non_blocking_findings_converges() {
        assert_eq!(
            decide_next_state(&[info_finding()], 1, 5, 1, 5),
            RunState::Converged
        );
    }

    #[test]
    fn blocking_reviewer_finding_within_review_budget_reboucles_to_coder() {
        assert_eq!(
            decide_next_state(&[blocking_finding()], 1, 5, 0, 5),
            RunState::CoderRunning
        );
    }

    #[test]
    fn blocking_reviewer_finding_at_review_budget_exceeds_max_review_cycles() {
        assert_eq!(
            decide_next_state(&[blocking_finding()], 5, 5, 0, 5),
            RunState::MaxReviewCyclesExceeded
        );
    }

    #[test]
    fn blocking_reviewer_finding_past_review_budget_exceeds_max_review_cycles() {
        assert_eq!(
            decide_next_state(&[blocking_finding()], 6, 5, 0, 5),
            RunState::MaxReviewCyclesExceeded
        );
    }

    /// The tampering finding (`FindingSource::Warden`) is charged to the
    /// review budget exactly like a `Reviewer`-sourced one.
    #[test]
    fn blocking_tampering_finding_is_charged_to_the_review_budget() {
        assert_eq!(
            decide_next_state(&[tampering_finding()], 5, 5, 0, 5),
            RunState::MaxReviewCyclesExceeded
        );
    }

    /// Decision #37 Q1, the core acceptance criterion of issue #43: a
    /// blocking tester finding reboucles/exhausts against the *test* budget,
    /// never the review budget -- even when the review budget is itself
    /// already exhausted, since a tester finding only ever appears in
    /// `findings` on a cycle whose review already came back clean this same
    /// cycle (`warden::orchestrator::run_convergence_loop`'s gate, issue
    /// #41).
    #[test]
    fn blocking_tester_finding_within_test_budget_reboucles_to_coder() {
        assert_eq!(
            decide_next_state(&[tester_blocking_finding()], 5, 5, 1, 5),
            RunState::CoderRunning
        );
    }

    #[test]
    fn blocking_tester_finding_at_test_budget_exceeds_max_test_cycles_not_review() {
        assert_eq!(
            decide_next_state(&[tester_blocking_finding()], 1, 5, 3, 3),
            RunState::MaxTestCyclesExceeded
        );
    }

    #[test]
    fn blocking_tester_finding_past_test_budget_exceeds_max_test_cycles() {
        assert_eq!(
            decide_next_state(&[tester_blocking_finding()], 1, 5, 4, 3),
            RunState::MaxTestCyclesExceeded
        );
    }

    /// A scoped re-review (issue #41/#42) triggered by a *tester* finding's
    /// correctif is still a `Reviewer`-sourced finding when it itself finds
    /// something -- decision #37 Q1's whole point: that reboucle is charged
    /// to the review budget, not the test budget, regardless of what
    /// motivated the coder's correctif in the first place.
    #[test]
    fn a_scoped_re_review_finding_after_a_tester_reboucle_is_charged_to_the_review_budget_not_test()
    {
        assert_eq!(
            decide_next_state(&[blocking_finding()], 2, 5, 1, 1),
            RunState::CoderRunning,
            "review budget still has room even though the test budget is already exhausted"
        );
    }

    #[test]
    fn parse_findings_empty_stdout_is_no_findings() {
        assert_eq!(parse_findings("").unwrap(), Vec::new());
        assert_eq!(parse_findings("   \n").unwrap(), Vec::new());
    }

    #[test]
    fn parse_findings_happy_path() {
        // NDJSON wire format (code-standards.md "Agent Subprocess
        // Protocol"): one finding object per line, no wrapping array.
        let stdout = r#"{"source":"tester","severity":"blocking","file":"src/main.rs","description":"test fails","action":"fix panic"}"#;
        let findings = parse_findings(stdout).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].source, FindingSource::Tester);
        assert_eq!(findings[0].severity, Severity::Blocking);
        assert_eq!(findings[0].file.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn parse_findings_multiple_lines_yield_multiple_findings() {
        // The defining property of NDJSON: each line is an independent
        // finding, so a reviewer raising several issues in one invocation
        // just emits several lines.
        let stdout = concat!(
            r#"{"source":"reviewer","severity":"blocking","description":"issue one"}"#,
            "\n",
            r#"{"source":"reviewer","severity":"warning","description":"issue two"}"#,
            "\n",
        );
        let findings = parse_findings(stdout).unwrap();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].description, "issue one");
        assert_eq!(findings[1].description, "issue two");
    }

    #[test]
    fn parse_findings_rejects_malformed_json() {
        assert!(parse_findings("not json").is_err());
    }

    #[test]
    fn parse_findings_rejects_unknown_severity() {
        let stdout = r#"{"source":"reviewer","severity":"catastrophic","description":"x"}"#;
        assert_eq!(
            parse_findings(stdout),
            Err(CoreError::UnknownSeverity("catastrophic".to_string()))
        );
    }

    #[test]
    fn parse_findings_rejects_unknown_source() {
        let stdout = r#"{"source":"ghost","severity":"info","description":"x"}"#;
        assert!(parse_findings(stdout).is_err());
    }

    #[test]
    fn parse_findings_blank_lines_between_findings_are_ignored() {
        // Reconciled for the NDJSON protocol (M3): the original intent of
        // this test — "an explicitly empty/no-content case still yields no
        // findings" — now maps onto blank-line handling rather than an
        // empty `findings` array, since there's no wrapping array anymore.
        let stdout = "\n   \n\n";
        assert_eq!(parse_findings(stdout).unwrap(), Vec::new());
    }

    #[test]
    fn parse_findings_rejects_missing_required_field() {
        // `description` is required by the wire protocol; an agent that
        // omits it (buggy or malicious output) must be a typed parse error,
        // not a panic or a silently-defaulted empty string.
        let stdout = r#"{"source":"reviewer","severity":"blocking"}"#;
        assert!(matches!(
            parse_findings(stdout),
            Err(CoreError::MalformedAgentOutput(_))
        ));
    }

    #[test]
    fn parse_findings_ignores_unknown_extra_fields_for_forward_compat() {
        let stdout = r#"{"source":"tester","severity":"info","description":"x","confidence":0.9}"#;
        let findings = parse_findings(stdout).unwrap();
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn parse_findings_rejects_trailing_noise_after_the_json_object() {
        // Reconciled for the NDJSON protocol (M3): a valid finding on the
        // first line followed by a stray non-JSON log line is exactly the
        // shape of "trailing noise" a real agent CLI can leak onto stdout.
        // We deliberately don't salvage the line(s) that did parse — a
        // stream that has shown itself to violate the protocol is treated
        // as untrustworthy in full, not partially recovered.
        let stdout = "{\"source\":\"reviewer\",\"severity\":\"info\",\"description\":\"ok\"}\nDEBUG: agent finished in 1.2s\n";
        assert!(matches!(
            parse_findings(stdout),
            Err(CoreError::MalformedAgentOutput(_))
        ));
    }

    #[test]
    fn parse_findings_rejects_a_top_level_json_array_instead_of_object() {
        assert!(parse_findings("[]").is_err());
    }

    // ---- validate_finding_sources_for_role (issue #24 review, cycle 2,
    // MAJOR 2) ----------------------------------------------------------

    fn finding_with_source(source: FindingSource) -> Finding {
        Finding {
            source,
            severity: Severity::Blocking,
            file: None,
            description: "x".to_string(),
            action: None,
        }
    }

    #[test]
    fn validate_finding_sources_for_role_accepts_a_reviewer_finding_with_the_reviewer_source() {
        let findings = vec![finding_with_source(FindingSource::Reviewer)];
        assert!(validate_finding_sources_for_role(&findings, AgentRole::Reviewer).is_ok());
    }

    #[test]
    fn validate_finding_sources_for_role_accepts_a_tester_finding_with_the_tester_source() {
        let findings = vec![finding_with_source(FindingSource::Tester)];
        assert!(validate_finding_sources_for_role(&findings, AgentRole::Tester).is_ok());
    }

    #[test]
    fn validate_finding_sources_for_role_accepts_no_findings_at_all() {
        assert!(validate_finding_sources_for_role(&[], AgentRole::Reviewer).is_ok());
        assert!(validate_finding_sources_for_role(&[], AgentRole::Tester).is_ok());
    }

    #[test]
    fn validate_finding_sources_for_role_rejects_a_reviewer_finding_claiming_the_warden_source() {
        let findings = vec![finding_with_source(FindingSource::Warden)];
        let error = validate_finding_sources_for_role(&findings, AgentRole::Reviewer).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentOutput(_)));
    }

    #[test]
    fn validate_finding_sources_for_role_rejects_a_reviewer_finding_claiming_the_ci_source() {
        let findings = vec![finding_with_source(FindingSource::Ci)];
        assert!(validate_finding_sources_for_role(&findings, AgentRole::Reviewer).is_err());
    }

    /// The sibling-role impersonation case: a tester claiming the
    /// reviewer's own source (issue #24 review Minor 2, `tester_succeeded`
    /// trusting an agent-controlled `source`).
    #[test]
    fn validate_finding_sources_for_role_rejects_a_tester_finding_claiming_the_reviewer_source() {
        let findings = vec![finding_with_source(FindingSource::Reviewer)];
        let error = validate_finding_sources_for_role(&findings, AgentRole::Tester).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentOutput(_)));
    }

    /// The whole batch is rejected on the first mismatch, even when an
    /// earlier finding in the same batch was legitimate -- no salvaging
    /// the parts that looked fine, mirroring `parse_findings`' own stance
    /// on a shape error.
    #[test]
    fn validate_finding_sources_for_role_rejects_the_whole_batch_on_one_bad_finding() {
        let findings = vec![
            finding_with_source(FindingSource::Reviewer),
            finding_with_source(FindingSource::Warden),
        ];
        assert!(validate_finding_sources_for_role(&findings, AgentRole::Reviewer).is_err());
    }

    #[test]
    fn validate_finding_sources_for_role_rejects_the_coder_role_defensively() {
        let error = validate_finding_sources_for_role(&[], AgentRole::Coder).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentOutput(_)));
    }

    #[test]
    fn decide_next_state_mixed_severities_still_reboucles_on_any_blocking() {
        let findings = vec![info_finding(), blocking_finding()];
        assert_eq!(
            decide_next_state(&findings, 1, 5, 0, 5),
            RunState::CoderRunning
        );
    }

    // ---- FindingSource::Ci -------------------------------------------------

    #[test]
    fn ci_finding_source_round_trips_through_its_string_form() {
        assert_eq!(FindingSource::Ci.as_str(), "ci");
        assert_eq!(FindingSource::parse("ci").unwrap(), FindingSource::Ci);
    }

    // ---- FindingSource::Warden (issue #24 review, M4) ----------------------

    #[test]
    fn warden_finding_source_round_trips_through_its_string_form() {
        assert_eq!(FindingSource::Warden.as_str(), "warden");
        assert_eq!(
            FindingSource::parse("warden").unwrap(),
            FindingSource::Warden
        );
    }

    // ---- decide_next_state_after_ci (issue #5) -----------------------------

    #[test]
    fn merged_and_checks_passed_both_reach_done() {
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::Merged, 1, 5),
            RunState::Done
        );
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::ChecksPassed, 1, 5),
            RunState::Done
        );
    }

    #[test]
    fn closed_without_merging_and_timed_out_both_fail_the_run() {
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::Closed, 1, 5),
            RunState::Failed
        );
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::TimedOut, 1, 5),
            RunState::Failed
        );
    }

    #[test]
    fn checks_failed_reboucles_to_coder_within_cycle_budget() {
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::ChecksFailed, 1, 5),
            RunState::CoderRunning
        );
    }

    #[test]
    fn checks_failed_at_cycle_budget_fails_the_run_not_a_max_cycles_exceeded_state() {
        // AwaitingCi's only legal next states are Done/CoderRunning/Failed
        // (state.rs) -- neither MaxReviewCyclesExceeded nor
        // MaxTestCyclesExceeded is reachable from here, unlike
        // decide_next_state's reviewer/tester equivalents.
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::ChecksFailed, 5, 5),
            RunState::Failed
        );
    }

    #[test]
    fn every_decide_next_state_after_ci_outcome_is_a_legal_awaiting_ci_transition() {
        for outcome in [
            CiOutcome::Merged,
            CiOutcome::ChecksPassed,
            CiOutcome::Closed,
            CiOutcome::ChecksFailed,
            CiOutcome::TimedOut,
        ] {
            let next = decide_next_state_after_ci(outcome, 1, 5);
            assert!(
                RunState::AwaitingCi.validate_transition(next).is_ok(),
                "{outcome:?} -> {next:?} is not a legal AwaitingCi transition"
            );
        }
    }
}
