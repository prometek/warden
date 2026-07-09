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

    #[test]
    fn decide_next_state_mixed_severities_still_reboucles_on_any_blocking() {
        let findings = vec![info_finding(), blocking_finding()];
        assert_eq!(decide_next_state(&findings, 1, 5), RunState::CoderRunning);
    }
}
