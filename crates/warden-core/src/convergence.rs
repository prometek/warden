//! Convergence rules: interpreting findings from a workflow step's agent and deciding the next
//! [`RunState`].

use serde::Deserialize;

use crate::error::{CoreError, Result};
use crate::state::RunState;
use crate::workflow::{Role, Workflow};

/// Which agent (or, for CI/Warden itself, which non-agent process) raised a finding
/// (`FINDINGS.source`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingSource {
    /// The workflow role name that raised this finding.
    Role(String),
    Ci,
    Warden,
}

impl FindingSource {
    pub const RESERVED_ROLE_NAMES: &'static [&'static str] = &["ci", "warden"];

    /// Convenience constructor for a role-sourced finding -- `FindingSource::role("techlead")`
    /// reads more directly at call sites than `FindingSource::Role("techlead".to_string())`.
    pub fn role(name: impl Into<String>) -> Self {
        FindingSource::Role(name.into())
    }

    pub fn as_str(&self) -> &str {
        match self {
            FindingSource::Role(name) => name,
            FindingSource::Ci => "ci",
            FindingSource::Warden => "warden",
        }
    }

    /// ****: roles are open (workflow-defined), so this no longer rejects an unrecognized name.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "ci" => Ok(FindingSource::Ci),
            "warden" => Ok(FindingSource::Warden),
            other if !other.trim().is_empty() => Ok(FindingSource::Role(other.to_string())),
            _ => Err(CoreError::UnknownFindingSource(raw.to_string())),
        }
    }
}

/// Severity of a finding (`FINDINGS.severity`).
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

/// A single finding raised by a workflow step's agent during a cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub source: FindingSource,
    pub severity: Severity,
    pub file: Option<String>,
    pub description: String,
    pub action: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawFinding {
    source: String,
    severity: String,
    file: Option<String>,
    description: String,
    action: Option<String>,
}

/// Parses an agent's stdout into a validated list of [`Finding`]s.
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

/// Rejects any finding whose `source` isn't the one `role` is entitled to claim.
pub fn validate_finding_sources_for_role(findings: &[Finding], expected_role: &Role) -> Result<()> {
    let expected = FindingSource::role(expected_role.as_str());
    for (index, finding) in findings.iter().enumerate() {
        if finding.source != expected {
            return Err(CoreError::MalformedAgentOutput(format!(
                "finding at index {index} claims source {:?}, but the {expected_role} role may \
                 only raise findings with source {:?}",
                finding.source.as_str(),
                expected.as_str(),
            )));
        }
    }
    Ok(())
}

/// Decides the next [`RunState`] once a workflow step's cycle findings are known.
pub fn decide_next_state_for_step(
    findings: &[Finding],
    workflow: &Workflow,
    step_index: u32,
    current_cycle: u32,
    max_cycles: u32,
) -> RunState {
    let step_role = &workflow.steps[step_index as usize].role;
    let expected_source = FindingSource::role(step_role.as_str());
    let blocking = findings.iter().any(|f| {
        f.severity == Severity::Blocking
            && (f.source == expected_source || f.source == FindingSource::Warden)
    });

    if !blocking {
        return if workflow.is_last_step(step_index) {
            RunState::Converged
        } else {
            RunState::RunningStep(step_index + 1)
        };
    }

    if current_cycle >= max_cycles {
        RunState::StepCyclesExceeded(step_index)
    } else {
        RunState::CoderRunning
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiOutcome {
    Merged,
    ChecksPassed,
    Closed,
    ChecksFailed,
    TimedOut,
}

/// Decides the next [`RunState`] once a run's CI watch reaches a terminal outcome.
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
    use crate::workflow::Workflow;

    fn default_workflow() -> Workflow {
        Workflow::builtin_default()
    }

    fn reviewer_role() -> Role {
        Role::new("reviewer").unwrap()
    }

    fn tester_role() -> Role {
        Role::new("tester").unwrap()
    }

    fn blocking_finding() -> Finding {
        Finding {
            source: FindingSource::role("reviewer"),
            severity: Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "unchecked unwrap".to_string(),
            action: Some("use ? instead".to_string()),
        }
    }

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
            source: FindingSource::role("tester"),
            severity: Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "test fails".to_string(),
            action: Some("fix the panic".to_string()),
        }
    }

    fn info_finding() -> Finding {
        Finding {
            source: FindingSource::role("tester"),
            severity: Severity::Info,
            file: None,
            description: "consider adding a doc comment".to_string(),
            action: None,
        }
    }

    #[test]
    fn no_findings_converges_on_the_last_step() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[], &workflow, 2, 1, 5),
            RunState::Converged
        );
    }

    #[test]
    fn no_findings_on_a_non_last_step_advances_to_the_next_step() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[], &workflow, 1, 1, 5),
            RunState::RunningStep(2)
        );
    }

    #[test]
    fn only_non_blocking_findings_advances() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[info_finding()], &workflow, 2, 1, 5),
            RunState::Converged
        );
    }

    #[test]
    fn blocking_finding_on_the_reviewer_step_within_budget_reboucles_to_coder() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[blocking_finding()], &workflow, 1, 1, 5),
            RunState::CoderRunning
        );
    }

    #[test]
    fn blocking_finding_on_the_reviewer_step_at_budget_exceeds_its_own_step_cycles() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[blocking_finding()], &workflow, 1, 5, 5),
            RunState::StepCyclesExceeded(1)
        );
    }

    #[test]
    fn blocking_finding_on_the_reviewer_step_past_budget_exceeds_its_own_step_cycles() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[blocking_finding()], &workflow, 1, 6, 5),
            RunState::StepCyclesExceeded(1)
        );
    }

    #[test]
    fn tampering_finding_is_charged_to_the_step_it_is_folded_into() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[tampering_finding()], &workflow, 1, 5, 5),
            RunState::StepCyclesExceeded(1)
        );
    }

    #[test]
    fn a_different_steps_finding_never_blocks_this_step() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[tester_blocking_finding()], &workflow, 1, 1, 5),
            RunState::RunningStep(2),
            "a tester-sourced finding must not block the reviewer step's own decision"
        );
    }

    #[test]
    fn blocking_tester_finding_within_its_own_budget_reboucles_to_coder() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[tester_blocking_finding()], &workflow, 2, 1, 5),
            RunState::CoderRunning
        );
    }

    #[test]
    fn blocking_tester_finding_at_its_own_budget_exceeds_step_cycles_not_the_reviewers() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[tester_blocking_finding()], &workflow, 2, 3, 3),
            RunState::StepCyclesExceeded(2)
        );
    }

    #[test]
    fn blocking_tester_finding_past_its_own_budget_exceeds_step_cycles() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[tester_blocking_finding()], &workflow, 2, 4, 3),
            RunState::StepCyclesExceeded(2)
        );
    }

    #[test]
    fn decide_next_state_mixed_severities_still_reboucles_on_any_blocking() {
        let workflow = default_workflow();
        let findings = vec![info_finding(), blocking_finding()];
        assert_eq!(
            decide_next_state_for_step(&findings, &workflow, 1, 1, 5),
            RunState::CoderRunning
        );
    }

    #[test]
    fn a_custom_role_beyond_the_default_pipeline_aggregates_like_any_other_step() {
        let yaml = r#"
name: with-techlead
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: code-reviewer
    gate: loop-until-clean
  - role: tester
    agent: test-runner
    gate: loop-until-clean
  - role: techlead
    agent: techlead
    gate: loop-until-clean
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();

        let techlead_blocking = Finding {
            source: FindingSource::role("techlead"),
            severity: Severity::Blocking,
            file: None,
            description: "architecture concern".to_string(),
            action: Some("reconsider the approach".to_string()),
        };
        assert_eq!(
            decide_next_state_for_step(&[techlead_blocking], &workflow, 3, 1, 5),
            RunState::CoderRunning
        );
        assert_eq!(
            decide_next_state_for_step(&[], &workflow, 3, 1, 5),
            RunState::Converged,
            "techlead is this workflow's last step, so a clean cycle converges"
        );
        assert_eq!(
            decide_next_state_for_step(&[], &workflow, 2, 1, 5),
            RunState::RunningStep(3),
            "tester is no longer the last step in this workflow, so a clean cycle advances \
             instead of converging"
        );
    }

    #[test]
    fn parse_findings_empty_stdout_is_no_findings() {
        assert_eq!(parse_findings("").unwrap(), Vec::new());
        assert_eq!(parse_findings("   \n").unwrap(), Vec::new());
    }

    #[test]
    fn parse_findings_happy_path() {
        let stdout = r#"{"source":"tester","severity":"blocking","file":"src/main.rs","description":"test fails","action":"fix panic"}"#;
        let findings = parse_findings(stdout).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].source, FindingSource::role("tester"));
        assert_eq!(findings[0].severity, Severity::Blocking);
        assert_eq!(findings[0].file.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn parse_findings_multiple_lines_yield_multiple_findings() {
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
    fn parse_findings_accepts_any_non_blank_source_as_an_open_role() {
        let stdout = r#"{"source":"ghost","severity":"info","description":"x"}"#;
        let findings = parse_findings(stdout).unwrap();
        assert_eq!(findings[0].source, FindingSource::role("ghost"));
    }

    #[test]
    fn parse_findings_rejects_a_blank_source() {
        let stdout = r#"{"source":"   ","severity":"info","description":"x"}"#;
        assert!(matches!(
            parse_findings(stdout),
            Err(CoreError::UnknownFindingSource(_))
        ));
    }

    #[test]
    fn parse_findings_blank_lines_between_findings_are_ignored() {
        let stdout = "\n   \n\n";
        assert_eq!(parse_findings(stdout).unwrap(), Vec::new());
    }

    #[test]
    fn parse_findings_rejects_missing_required_field() {
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
        let findings = vec![finding_with_source(FindingSource::role("reviewer"))];
        assert!(validate_finding_sources_for_role(&findings, &reviewer_role()).is_ok());
    }

    #[test]
    fn validate_finding_sources_for_role_accepts_a_tester_finding_with_the_tester_source() {
        let findings = vec![finding_with_source(FindingSource::role("tester"))];
        assert!(validate_finding_sources_for_role(&findings, &tester_role()).is_ok());
    }

    #[test]
    fn validate_finding_sources_for_role_accepts_a_custom_roles_own_finding() {
        let role = Role::new("techlead").unwrap();
        let findings = vec![finding_with_source(FindingSource::role("techlead"))];
        assert!(validate_finding_sources_for_role(&findings, &role).is_ok());
    }

    #[test]
    fn validate_finding_sources_for_role_accepts_no_findings_at_all() {
        assert!(validate_finding_sources_for_role(&[], &reviewer_role()).is_ok());
        assert!(validate_finding_sources_for_role(&[], &tester_role()).is_ok());
    }

    #[test]
    fn validate_finding_sources_for_role_rejects_a_reviewer_finding_claiming_the_warden_source() {
        let findings = vec![finding_with_source(FindingSource::Warden)];
        let error = validate_finding_sources_for_role(&findings, &reviewer_role()).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentOutput(_)));
    }

    #[test]
    fn validate_finding_sources_for_role_rejects_a_reviewer_finding_claiming_the_ci_source() {
        let findings = vec![finding_with_source(FindingSource::Ci)];
        assert!(validate_finding_sources_for_role(&findings, &reviewer_role()).is_err());
    }

    #[test]
    fn validate_finding_sources_for_role_rejects_a_tester_finding_claiming_the_reviewer_source() {
        let findings = vec![finding_with_source(FindingSource::role("reviewer"))];
        let error = validate_finding_sources_for_role(&findings, &tester_role()).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentOutput(_)));
    }

    #[test]
    fn validate_finding_sources_for_role_rejects_the_whole_batch_on_one_bad_finding() {
        let findings = vec![
            finding_with_source(FindingSource::role("reviewer")),
            finding_with_source(FindingSource::Warden),
        ];
        assert!(validate_finding_sources_for_role(&findings, &reviewer_role()).is_err());
    }

    #[test]
    fn ci_finding_source_round_trips_through_its_string_form() {
        assert_eq!(FindingSource::Ci.as_str(), "ci");
        assert_eq!(FindingSource::parse("ci").unwrap(), FindingSource::Ci);
    }

    #[test]
    fn warden_finding_source_round_trips_through_its_string_form() {
        assert_eq!(FindingSource::Warden.as_str(), "warden");
        assert_eq!(
            FindingSource::parse("warden").unwrap(),
            FindingSource::Warden
        );
    }

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
    fn checks_failed_at_cycle_budget_fails_the_run_not_a_step_cycles_exceeded_state() {
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
                RunState::AwaitingCi.validate_transition(next, 3).is_ok(),
                "{outcome:?} -> {next:?} is not a legal AwaitingCi transition"
            );
        }
    }
}
