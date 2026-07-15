//! Wire form of the JSON payload `warden` feeds an agent subprocess over its
//! stdin (ADR-0012, issue #20 Scope B): the run intent for the coder, or the
//! target commit/diff/prior-cycle findings for the reviewer/tester, plus the
//! agent's own role in every payload. This is the channel
//! code-standards.md's Agent Subprocess Protocol already sanctions
//! ("Échange JSON en streaming sur stdin/stdout"), but `process::spawn`
//! never actually fed one until this issue -- previously the coder received
//! no warden-managed context at all, and the reviewer/tester received none
//! either.
//!
//! Pure/serializable shape only -- the I/O (writing to a child's stdin,
//! closing it) lives in `warden::process`, mirroring the
//! `warden_core::CiResultMessage` / `warden::ci_channel` split. Constructed
//! here with the same "wire struct + typed constructor + validated parse"
//! convention as `ci_channel`/`evidence_wire`: `AgentInputMessage` is never
//! built by hand from raw strings, and any round-trip through JSON is
//! reparsed and validated, never partially trusted.

use serde::{Deserialize, Serialize};

use crate::convergence::{Finding, FindingSource, Severity};
use crate::error::{CoreError, Result};
use crate::state::AgentRole;

/// Current version of the agent input payload. `parse_agent_input_message`
/// rejects any other value outright rather than guessing at forward/backward
/// compatibility -- bump this if the shape changes in a way an agent-side
/// consumer must branch on.
pub const AGENT_INPUT_VERSION: u32 = 1;

/// Appended to [`AgentInputMessage::diff`] when `warden` truncated it at its
/// size cap (`warden::orchestrator::MAX_DIFF_BYTES`) before handing it to a
/// reviewer/tester (M1, issue #20 review). Lives here rather than as a
/// private const in `warden::orchestrator` (fix cycle 2, issue #20 review,
/// BUG 4) because this is where an agent-side consumer of the wire contract
/// actually looks: a marker an agent needs to detect but that isn't
/// documented on the field it appears in isn't part of the contract at all,
/// silently cutting the diff without a discoverable marker would be its own
/// silent fallback.
pub const DIFF_TRUNCATED_MARKER: &str = "\n\n[warden: diff truncated at the 8 MiB payload cap]\n";

/// Wire shape of one [`Finding`] riding inside the agent input payload --
/// mirrors `ci_channel::CiFindingWire`'s convention (the already-validated
/// `as_str()` string form for `source`/`severity`, re-validated back into the
/// real enums at receipt): `Finding` itself doesn't derive
/// `Serialize`/`Deserialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AgentFindingWire {
    source: String,
    severity: String,
    file: Option<String>,
    description: String,
    action: Option<String>,
}

impl AgentFindingWire {
    fn from_finding(finding: &Finding) -> Self {
        Self {
            source: finding.source.as_str().to_string(),
            severity: finding.severity.as_str().to_string(),
            file: finding.file.clone(),
            description: finding.description.clone(),
            action: finding.action.clone(),
        }
    }

    fn into_finding(self) -> Result<Finding> {
        Ok(Finding {
            source: FindingSource::parse(&self.source)?,
            severity: Severity::parse(&self.severity)?,
            file: self.file,
            description: self.description,
            action: self.action,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentInputWire {
    version: u32,
    role: String,
    intent: Option<String>,
    target_commit: Option<String>,
    diff: Option<String>,
    findings: Vec<AgentFindingWire>,
}

/// The payload `warden` feeds an agent subprocess over stdin (ADR-0012): the
/// run intent for the coder, or the target commit/diff/prior findings for
/// the reviewer/tester -- never a mix of both, enforced by the two
/// constructors below rather than left to callers to assemble field-by-field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInputMessage {
    pub role: AgentRole,
    pub intent: Option<String>,
    pub target_commit: Option<String>,
    /// Reviewer/tester only (`None` for coder input, [`Self::for_coder`]):
    /// `git diff base..target` for this cycle's changes. An empty string
    /// is a legitimate value in its own right (a cycle whose coder
    /// committed no changes), distinct from `None`. May be truncated at
    /// `warden::orchestrator::MAX_DIFF_BYTES` -- a truncated diff has
    /// [`DIFF_TRUNCATED_MARKER`] appended, so a reviewer/tester agent can
    /// tell a truncated diff from a genuinely small one rather than acting
    /// on a silently incomplete payload.
    pub diff: Option<String>,
    pub findings: Vec<Finding>,
}

impl AgentInputMessage {
    /// Coder input (issue #20 Scope B): the run intent is the only warden-
    /// managed context this scope propagates to the coder. Findings-driven
    /// coder re-invocation is a separate, not-yet-built concern -- reviewer/
    /// tester get commit/diff/findings instead, via [`Self::for_finding_agent`].
    ///
    /// Rejects a blank (empty or all-whitespace) `intent` with the same
    /// rigor [`parse_agent_input_message`] applies on the read side (M2,
    /// issue #20 review) -- without this, `to_json` could hand
    /// `parse_agent_input_message`'s own caller a payload the parser
    /// refuses to accept, since it enforces exactly this invariant.
    pub fn for_coder(intent: impl Into<String>) -> Result<Self> {
        let intent = intent.into();
        if intent.trim().is_empty() {
            return Err(CoreError::MalformedAgentInput(
                "coder input intent must not be blank".to_string(),
            ));
        }
        Ok(Self {
            role: AgentRole::Coder,
            intent: Some(intent),
            target_commit: None,
            diff: None,
            findings: Vec::new(),
        })
    }

    /// Reviewer/tester input: the commit under review, the diff this cycle's
    /// coder introduced against the cycle's starting commit, and the
    /// findings that triggered this cycle (including CI findings on a
    /// post-convergence reboucle, ADR-0011) -- empty on a run's first cycle.
    ///
    /// Rejects a blank `target_commit` with the same rigor
    /// [`parse_agent_input_message`] applies on the read side (M2, issue
    /// #20 review). `diff` is deliberately not validated the same way -- an
    /// empty diff (a cycle whose coder committed no changes) is a
    /// legitimate value, mirrored by the parser accepting an absent `diff`
    /// as `""`.
    pub fn for_finding_agent(
        role: AgentRole,
        target_commit: impl Into<String>,
        diff: impl Into<String>,
        findings: Vec<Finding>,
    ) -> Result<Self> {
        if role == AgentRole::Coder {
            return Err(CoreError::MalformedAgentInput(
                "for_finding_agent must be called with Reviewer or Tester, not Coder".to_string(),
            ));
        }
        let target_commit = target_commit.into();
        if target_commit.trim().is_empty() {
            return Err(CoreError::MalformedAgentInput(format!(
                "{} input target_commit must not be blank",
                role.as_str()
            )));
        }
        Ok(Self {
            role,
            intent: None,
            target_commit: Some(target_commit),
            diff: Some(diff.into()),
            findings,
        })
    }

    /// Serializes to the exact wire form [`parse_agent_input_message`] parses
    /// back -- one JSON object written to the agent's stdin, then the write
    /// half is closed (`warden::process::spawn`/`wait`), never left open
    /// waiting for more input.
    pub fn to_json(&self) -> Result<String> {
        let wire = AgentInputWire {
            version: AGENT_INPUT_VERSION,
            role: self.role.as_str().to_string(),
            intent: self.intent.clone(),
            target_commit: self.target_commit.clone(),
            diff: self.diff.clone(),
            findings: self
                .findings
                .iter()
                .map(AgentFindingWire::from_finding)
                .collect(),
        };
        serde_json::to_string(&wire)
            .map_err(|error| CoreError::MalformedAgentInput(error.to_string()))
    }
}

/// Parses one agent-input JSON payload, with the same rigor as
/// `parse_ci_result_message`/`parse_findings`: malformed JSON, an unsupported
/// `version`, an unknown `role`, an unparsable embedded finding, or a
/// role/field combination that violates the invariant `for_coder`/
/// `for_finding_agent` enforce at construction (e.g. a `coder` payload
/// missing `intent`, or a `reviewer`/`tester` payload missing
/// `target_commit`) are all typed errors -- never silently defaulted
/// (code-standards.md: "valider toute entrée externe ... à la frontière").
pub fn parse_agent_input_message(raw: &str) -> Result<AgentInputMessage> {
    let wire: AgentInputWire = serde_json::from_str(raw)
        .map_err(|error| CoreError::MalformedAgentInput(error.to_string()))?;

    if wire.version != AGENT_INPUT_VERSION {
        return Err(CoreError::MalformedAgentInput(format!(
            "unsupported agent input version {} (expected {AGENT_INPUT_VERSION})",
            wire.version
        )));
    }

    let role = AgentRole::parse(&wire.role)?;
    let findings = wire
        .findings
        .into_iter()
        .map(AgentFindingWire::into_finding)
        .collect::<Result<Vec<_>>>()?;

    match role {
        AgentRole::Coder => {
            let intent = wire
                .intent
                .filter(|intent| !intent.trim().is_empty())
                .ok_or_else(|| {
                    CoreError::MalformedAgentInput(
                        "coder input is missing a non-blank intent".to_string(),
                    )
                })?;
            Ok(AgentInputMessage {
                role,
                intent: Some(intent),
                target_commit: None,
                diff: None,
                findings,
            })
        }
        AgentRole::Reviewer | AgentRole::Tester => {
            let target_commit = wire
                .target_commit
                .filter(|commit| !commit.trim().is_empty())
                .ok_or_else(|| {
                    CoreError::MalformedAgentInput(format!(
                        "{} input is missing a non-blank target_commit",
                        role.as_str()
                    ))
                })?;
            Ok(AgentInputMessage {
                role,
                intent: None,
                target_commit: Some(target_commit),
                // Absent `diff` is treated as "no diff" (empty string)
                // rather than an error -- a cycle whose coder made no
                // changes has a legitimately empty diff to report.
                diff: Some(wire.diff.unwrap_or_default()),
                findings,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_finding() -> Finding {
        Finding {
            source: FindingSource::Ci,
            severity: Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "build failed".to_string(),
            action: Some("fix the build".to_string()),
        }
    }

    #[test]
    fn coder_input_round_trips_through_json() {
        let message = AgentInputMessage::for_coder("implement the thing").unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();
        assert_eq!(decoded, message);
        assert_eq!(decoded.role, AgentRole::Coder);
        assert_eq!(decoded.intent.as_deref(), Some("implement the thing"));
        assert!(decoded.target_commit.is_none());
        assert!(decoded.diff.is_none());
        assert!(decoded.findings.is_empty());
    }

    #[test]
    fn finding_agent_input_round_trips_through_json_with_findings() {
        let message = AgentInputMessage::for_finding_agent(
            AgentRole::Reviewer,
            "abc123",
            "diff --git a/x b/x\n+added line\n",
            vec![sample_finding()],
        )
        .unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();

        assert_eq!(decoded, message);
        assert_eq!(decoded.role, AgentRole::Reviewer);
        assert!(decoded.intent.is_none());
        assert_eq!(decoded.target_commit.as_deref(), Some("abc123"));
        assert_eq!(
            decoded.diff.as_deref(),
            Some("diff --git a/x b/x\n+added line\n")
        );
        assert_eq!(decoded.findings.len(), 1);
        assert_eq!(decoded.findings[0].source, FindingSource::Ci);
    }

    #[test]
    fn tester_input_with_no_prior_findings_round_trips_to_an_empty_list() {
        let message =
            AgentInputMessage::for_finding_agent(AgentRole::Tester, "abc123", "", Vec::new())
                .unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();
        assert!(decoded.findings.is_empty());
        assert_eq!(decoded.diff.as_deref(), Some(""));
    }

    #[test]
    fn for_finding_agent_rejects_the_coder_role() {
        let result = AgentInputMessage::for_finding_agent(AgentRole::Coder, "abc123", "", vec![]);
        assert!(matches!(result, Err(CoreError::MalformedAgentInput(_))));
    }

    /// M2 (issue #20 review): construction must validate with the same
    /// rigor `parse_agent_input_message` applies on the read side --
    /// otherwise `for_coder` could hand its own caller a payload
    /// `to_json`+`parse_agent_input_message` round-trips into something the
    /// parser rejects outright.
    #[test]
    fn for_coder_rejects_a_blank_intent() {
        assert!(matches!(
            AgentInputMessage::for_coder(""),
            Err(CoreError::MalformedAgentInput(_))
        ));
        assert!(matches!(
            AgentInputMessage::for_coder("   \n\t "),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    /// M2 counterpart for the reviewer/tester constructor.
    #[test]
    fn for_finding_agent_rejects_a_blank_target_commit() {
        assert!(matches!(
            AgentInputMessage::for_finding_agent(AgentRole::Reviewer, "", "", vec![]),
            Err(CoreError::MalformedAgentInput(_))
        ));
        assert!(matches!(
            AgentInputMessage::for_finding_agent(AgentRole::Tester, "   ", "", vec![]),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(matches!(
            parse_agent_input_message("not json"),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_an_unsupported_version() {
        let json = r#"{"version":99,"role":"coder","intent":"x","target_commit":null,"diff":null,"findings":[]}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_role() {
        let json = r#"{"version":1,"role":"ghost","intent":"x","target_commit":null,"diff":null,"findings":[]}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::UnknownRole(_))
        ));
    }

    #[test]
    fn rejects_a_coder_payload_missing_intent() {
        let json = r#"{"version":1,"role":"coder","intent":null,"target_commit":null,"diff":null,"findings":[]}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_a_reviewer_payload_missing_target_commit() {
        let json = r#"{"version":1,"role":"reviewer","intent":null,"target_commit":null,"diff":null,"findings":[]}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_finding_source_inside_findings() {
        let json = r#"{"version":1,"role":"tester","intent":null,"target_commit":"abc","diff":"","findings":[{"source":"ghost","severity":"blocking","file":null,"description":"x","action":null}]}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::UnknownFindingSource(_))
        ));
    }
}
