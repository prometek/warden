use serde::Deserialize;

use crate::error::{CoreError, Result};
use crate::state::RunState;
use crate::workflow::{Role, StepOutcome, StepTarget, Workflow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingSource {
    Role(String),
    Ci,
    Warden,
}

impl FindingSource {
    pub const RESERVED_ROLE_NAMES: &'static [&'static str] = &["ci", "warden"];

    pub fn role(name: impl Into<String>) -> Self {
        Self::Role(name.into())
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Role(name) => name,
            Self::Ci => "ci",
            Self::Warden => "warden",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "ci" => Ok(Self::Ci),
            "warden" => Ok(Self::Warden),
            other if !other.trim().is_empty() => Ok(Self::Role(other.to_string())),
            _ => Err(CoreError::UnknownFindingSource(raw.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Blocking,
    Warning,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "blocking" => Ok(Self::Blocking),
            "warning" => Ok(Self::Warning),
            "info" => Ok(Self::Info),
            other => Err(CoreError::UnknownSeverity(other.to_string())),
        }
    }
}

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

pub fn parse_findings(agent_stdout: &str) -> Result<Vec<Finding>> {
    agent_stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let raw: RawFinding = serde_json::from_str(line).map_err(|error| {
                CoreError::MalformedAgentOutput(format!("invalid JSON line {line:?}: {error}"))
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

pub fn validate_finding_sources_for_role(findings: &[Finding], expected_role: &Role) -> Result<()> {
    let expected = FindingSource::role(expected_role.as_str());
    for (index, finding) in findings.iter().enumerate() {
        if finding.source != expected {
            return Err(CoreError::MalformedAgentOutput(format!(
                "finding at index {index} claims source {:?}, but step {expected_role} may only raise findings with source {:?}",
                finding.source.as_str(),
                expected.as_str(),
            )));
        }
    }
    Ok(())
}

pub fn decide_next_state_for_step(
    findings: &[Finding],
    workflow: &Workflow,
    step_index: u32,
    current_cycle: u32,
    global_max_cycles: u32,
) -> RunState {
    let step = &workflow.steps[step_index as usize];
    let expected_source = FindingSource::role(step.role.as_str());
    let blocking = findings.iter().any(|finding| {
        finding.severity == Severity::Blocking
            && (finding.source == expected_source || finding.source == FindingSource::Warden)
    });
    let outcome = if blocking {
        StepOutcome::Blocking
    } else {
        StepOutcome::Clean
    };
    if blocking
        && current_cycle
            >= step
                .max_cycles
                .unwrap_or(global_max_cycles)
                .min(global_max_cycles)
    {
        return RunState::StepCyclesExceeded(step_index);
    }
    state_for_target(workflow.target_for(step_index, outcome))
}

pub fn state_for_target(target: StepTarget) -> RunState {
    match target {
        StepTarget::Step(index) => RunState::RunningStep(index),
        StepTarget::Converged => RunState::Converged,
        StepTarget::Failed => RunState::Failed,
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

pub fn decide_next_state_after_ci(
    outcome: CiOutcome,
    workflow_entry: u32,
    current_cycle: u32,
    max_cycles: u32,
) -> RunState {
    match outcome {
        CiOutcome::Merged | CiOutcome::ChecksPassed => RunState::Done,
        CiOutcome::Closed | CiOutcome::TimedOut => RunState::Failed,
        CiOutcome::ChecksFailed if current_cycle < max_cycles => {
            RunState::RunningStep(workflow_entry)
        }
        CiOutcome::ChecksFailed => RunState::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRAPH: &str = r#"
name: graph
entry: implement
steps:
  implement:
    type: agent
    agent: writer
    on_clean: verify
    on_blocking: implement
    on_error: failed
  verify:
    type: agent
    agent: verifier
    on_clean: converged
    on_blocking: implement
    on_error: failed
    max_cycles: 2
"#;

    fn finding(source: &str, severity: Severity) -> Finding {
        Finding {
            source: FindingSource::role(source),
            severity,
            file: None,
            description: "result".to_string(),
            action: None,
        }
    }

    #[test]
    fn clean_and_blocking_follow_explicit_edges() {
        let workflow = Workflow::parse_yaml(GRAPH).unwrap();
        let verify = workflow.step_index("verify").unwrap();
        assert_eq!(
            decide_next_state_for_step(&[], &workflow, verify, 1, 5),
            RunState::Converged
        );
        assert_eq!(
            decide_next_state_for_step(
                &[finding("verify", Severity::Blocking)],
                &workflow,
                verify,
                1,
                5,
            ),
            RunState::RunningStep(workflow.entry())
        );
    }

    #[test]
    fn per_step_budget_caps_global_budget() {
        let workflow = Workflow::parse_yaml(GRAPH).unwrap();
        let verify = workflow.step_index("verify").unwrap();
        assert_eq!(
            decide_next_state_for_step(
                &[finding("verify", Severity::Blocking)],
                &workflow,
                verify,
                2,
                5,
            ),
            RunState::StepCyclesExceeded(verify)
        );
    }

    #[test]
    fn findings_parse_and_validate_for_open_roles() {
        let raw = r#"{"source":"security","severity":"warning","description":"check"}"#;
        let findings = parse_findings(raw).unwrap();
        assert_eq!(findings[0].source, FindingSource::role("security"));
        assert!(
            validate_finding_sources_for_role(&findings, &Role::new("security").unwrap()).is_ok()
        );
    }

    #[test]
    fn malformed_findings_are_rejected() {
        assert!(parse_findings("not json").is_err());
        assert!(
            parse_findings(r#"{"source":"security","severity":"unknown","description":"x"}"#)
                .is_err()
        );
    }

    #[test]
    fn ci_failure_restarts_explicit_entry() {
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::ChecksFailed, 3, 1, 5),
            RunState::RunningStep(3)
        );
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::ChecksFailed, 3, 5, 5),
            RunState::Failed
        );
    }
}
