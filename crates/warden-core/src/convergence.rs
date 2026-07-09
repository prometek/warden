//! Convergence rules: interpreting findings from reviewer/tester agents and
//! deciding the next [`RunState`]. Pure logic — no I/O, no clock, no
//! subprocess. Parsing of agent stdout also lives here since it's the
//! boundary where untrusted external input is validated before it can ever
//! reach the state machine (code-standards.md, "Validation à la frontière").

use serde::Deserialize;

use crate::error::{CoreError, Result};
use crate::state::RunState;

/// Which agent raised a finding (`FINDINGS.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingSource {
    Reviewer,
    Tester,
}

impl FindingSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingSource::Reviewer => "reviewer",
            FindingSource::Tester => "tester",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "reviewer" => Ok(FindingSource::Reviewer),
            "tester" => Ok(FindingSource::Tester),
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

/// Wire format emitted by a reviewer/tester agent on stdout: one JSON object
/// with a `findings` array. Field names/values are attacker-controlled
/// (agent output is untrusted, code-standards.md "Agent Subprocess
/// Protocol") so every value is validated against a closed set here, never
/// passed through as a free-form string.
#[derive(Debug, Deserialize)]
struct RawFindingsReport {
    findings: Vec<RawFinding>,
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
///
/// A line that isn't parsable JSON, or whose `severity`/`source` isn't a
/// known value, is a [`CoreError::MalformedAgentOutput`] — never a panic —
/// per code-standards.md: "Ne jamais faire confiance à la sortie d'un agent
/// CLI". The caller (the orchestrator) is expected to turn a parse failure
/// into a blocking finding of its own, not to crash the run.
pub fn parse_findings(agent_stdout: &str) -> Result<Vec<Finding>> {
    let trimmed = agent_stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let report: RawFindingsReport = serde_json::from_str(trimmed)
        .map_err(|e| CoreError::MalformedAgentOutput(e.to_string()))?;

    report
        .findings
        .into_iter()
        .map(|raw| {
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

/// Decides the next [`RunState`] once reviewer + tester findings for a cycle
/// are known. Only meaningful from [`RunState::AwaitingReviewTest`]; callers
/// elsewhere in the state machine (crash recovery, `MaxCyclesExceeded` ->
/// `Failed`, ...) do not go through this function.
///
/// - Any blocking finding, with cycles remaining: `CoderRunning` (reboucle).
/// - Any blocking finding, at the cycle budget: `MaxCyclesExceeded`.
/// - No blocking finding: `Converged`.
pub fn decide_next_state(findings: &[Finding], current_cycle: u32, max_cycles: u32) -> RunState {
    let has_blocking = findings.iter().any(|f| f.severity == Severity::Blocking);

    if !has_blocking {
        return RunState::Converged;
    }

    if current_cycle >= max_cycles {
        RunState::MaxCyclesExceeded
    } else {
        RunState::CoderRunning
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
        assert_eq!(decide_next_state(&[], 1, 5), RunState::Converged);
    }

    #[test]
    fn only_non_blocking_findings_converges() {
        assert_eq!(
            decide_next_state(&[info_finding()], 1, 5),
            RunState::Converged
        );
    }

    #[test]
    fn blocking_finding_within_budget_reboucles_to_coder() {
        assert_eq!(
            decide_next_state(&[blocking_finding()], 1, 5),
            RunState::CoderRunning
        );
    }

    #[test]
    fn blocking_finding_at_budget_exceeds_max_cycles() {
        assert_eq!(
            decide_next_state(&[blocking_finding()], 5, 5),
            RunState::MaxCyclesExceeded
        );
    }

    #[test]
    fn blocking_finding_past_budget_exceeds_max_cycles() {
        assert_eq!(
            decide_next_state(&[blocking_finding()], 6, 5),
            RunState::MaxCyclesExceeded
        );
    }

    #[test]
    fn parse_findings_empty_stdout_is_no_findings() {
        assert_eq!(parse_findings("").unwrap(), Vec::new());
        assert_eq!(parse_findings("   \n").unwrap(), Vec::new());
    }

    #[test]
    fn parse_findings_happy_path() {
        let stdout = r#"{"findings":[{"source":"tester","severity":"blocking","file":"src/main.rs","description":"test fails","action":"fix panic"}]}"#;
        let findings = parse_findings(stdout).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].source, FindingSource::Tester);
        assert_eq!(findings[0].severity, Severity::Blocking);
        assert_eq!(findings[0].file.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn parse_findings_rejects_malformed_json() {
        assert!(parse_findings("not json").is_err());
    }

    #[test]
    fn parse_findings_rejects_unknown_severity() {
        let stdout =
            r#"{"findings":[{"source":"reviewer","severity":"catastrophic","description":"x"}]}"#;
        assert_eq!(
            parse_findings(stdout),
            Err(CoreError::UnknownSeverity("catastrophic".to_string()))
        );
    }

    #[test]
    fn parse_findings_rejects_unknown_source() {
        let stdout = r#"{"findings":[{"source":"ghost","severity":"info","description":"x"}]}"#;
        assert!(parse_findings(stdout).is_err());
    }
}
