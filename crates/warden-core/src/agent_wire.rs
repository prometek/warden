use serde::{Deserialize, Serialize};

use crate::convergence::{Finding, FindingSource, Severity};
use crate::error::{CoreError, Result};
use crate::state::AgentRole;

pub const AGENT_INPUT_VERSION: u32 = 3;

pub const DIFF_TRUNCATED_MARKER: &str = "\n\n[warden: diff truncated at the 8 MiB payload cap]\n";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewScope {
    /// This cycle's full diff/findings context -- the only mode before.
    Full,
    /// `diff`/`findings` are narrowed to a single correctif.
    Correctif,
}

impl ReviewScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewScope::Full => "full",
            ReviewScope::Correctif => "correctif",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "full" => Ok(ReviewScope::Full),
            "correctif" => Ok(ReviewScope::Correctif),
            other => Err(CoreError::MalformedAgentInput(format!(
                "unknown review scope {other:?} (expected \"full\" or \"correctif\")"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentInputWire {
    version: u32,
    role: String,
    /// required for every role. A payload without one is malformed, not a payload with an empty
    /// prompt -- serde's own "missing field" error is surfaced as `MalformedAgentInput`.
    system_prompt: String,
    intent: Option<String>,
    target_commit: Option<String>,
    diff: Option<String>,
    findings: Vec<AgentFindingWire>,
    scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInputMessage {
    pub role: AgentRole,
    /// The role's system prompt, from the markdown agent definition this invocation was built from.
    pub system_prompt: String,
    pub intent: Option<String>,
    pub target_commit: Option<String>,
    pub diff: Option<String>,
    pub findings: Vec<Finding>,
    pub scope: ReviewScope,
}

impl AgentInputMessage {
    /// Coder input: the run intent, plus the findings that triggered this cycle -- the ones the
    /// coder is being asked to fix.
    pub fn for_coder(
        system_prompt: impl Into<String>,
        intent: impl Into<String>,
        findings: Vec<Finding>,
    ) -> Result<Self> {
        let system_prompt = validate_system_prompt(AgentRole::Coder, system_prompt.into())?;
        let intent = intent.into();
        if intent.trim().is_empty() {
            return Err(CoreError::MalformedAgentInput(
                "coder input intent must not be blank".to_string(),
            ));
        }
        Ok(Self {
            role: AgentRole::Coder,
            system_prompt,
            intent: Some(intent),
            target_commit: None,
            diff: None,
            findings,
            scope: ReviewScope::Full,
        })
    }

    pub fn for_finding_agent(
        role: AgentRole,
        system_prompt: impl Into<String>,
        target_commit: impl Into<String>,
        diff: impl Into<String>,
        findings: Vec<Finding>,
    ) -> Result<Self> {
        if role == AgentRole::Coder {
            return Err(CoreError::MalformedAgentInput(
                "for_finding_agent must be called with Reviewer or Tester, not Coder".to_string(),
            ));
        }
        let system_prompt = validate_system_prompt(role, system_prompt.into())?;
        let target_commit = target_commit.into();
        if target_commit.trim().is_empty() {
            return Err(CoreError::MalformedAgentInput(format!(
                "{} input target_commit must not be blank",
                role.as_str()
            )));
        }
        Ok(Self {
            role,
            system_prompt,
            intent: None,
            target_commit: Some(target_commit),
            diff: Some(diff.into()),
            findings,
            scope: ReviewScope::Full,
        })
    }

    pub fn for_scoped_review(
        system_prompt: impl Into<String>,
        target_commit: impl Into<String>,
        correctif_diff: impl Into<String>,
        originating_findings: Vec<Finding>,
    ) -> Result<Self> {
        let mut message = Self::for_finding_agent(
            AgentRole::Reviewer,
            system_prompt,
            target_commit,
            correctif_diff,
            originating_findings,
        )?;
        message.scope = ReviewScope::Correctif;
        Ok(message)
    }

    /// Serializes to the exact wire form [`parse_agent_input_message`] parses back.
    pub fn to_json(&self) -> Result<String> {
        let wire = AgentInputWire {
            version: AGENT_INPUT_VERSION,
            role: self.role.as_str().to_string(),
            system_prompt: self.system_prompt.clone(),
            intent: self.intent.clone(),
            target_commit: self.target_commit.clone(),
            diff: self.diff.clone(),
            findings: self
                .findings
                .iter()
                .map(AgentFindingWire::from_finding)
                .collect(),
            scope: self.scope.as_str().to_string(),
        };
        serde_json::to_string(&wire)
            .map_err(|error| CoreError::MalformedAgentInput(error.to_string()))
    }
}

pub fn build_finding_agent_input_json(
    role_name: &str,
    system_prompt: impl Into<String>,
    target_commit: impl Into<String>,
    diff: impl Into<String>,
    findings: Vec<Finding>,
    scope: ReviewScope,
) -> Result<String> {
    if role_name.trim().is_empty() {
        return Err(CoreError::MalformedAgentInput(
            "step input role must not be blank".to_string(),
        ));
    }
    let system_prompt = system_prompt.into();
    if system_prompt.trim().is_empty() {
        return Err(CoreError::MalformedAgentInput(format!(
            "{role_name} input system_prompt must not be blank"
        )));
    }
    let target_commit = target_commit.into();
    if target_commit.trim().is_empty() {
        return Err(CoreError::MalformedAgentInput(format!(
            "{role_name} input target_commit must not be blank"
        )));
    }

    let wire = AgentInputWire {
        version: AGENT_INPUT_VERSION,
        role: role_name.to_string(),
        system_prompt,
        intent: None,
        target_commit: Some(target_commit),
        diff: Some(diff.into()),
        findings: findings
            .iter()
            .map(AgentFindingWire::from_finding)
            .collect(),
        scope: scope.as_str().to_string(),
    };
    serde_json::to_string(&wire).map_err(|error| CoreError::MalformedAgentInput(error.to_string()))
}

/// The producer-shaped sibling of [`build_finding_agent_input_json`]: the stdin JSON payload for
/// `workflow.steps[0]`.
pub fn build_producer_input_json(
    role_name: &str,
    system_prompt: impl Into<String>,
    intent: impl Into<String>,
    findings: Vec<Finding>,
) -> Result<String> {
    if role_name.trim().is_empty() {
        return Err(CoreError::MalformedAgentInput(
            "step input role must not be blank".to_string(),
        ));
    }
    let system_prompt = system_prompt.into();
    if system_prompt.trim().is_empty() {
        return Err(CoreError::MalformedAgentInput(format!(
            "{role_name} input system_prompt must not be blank"
        )));
    }
    let intent = intent.into();
    if intent.trim().is_empty() {
        return Err(CoreError::MalformedAgentInput(format!(
            "{role_name} input intent must not be blank"
        )));
    }

    let wire = AgentInputWire {
        version: AGENT_INPUT_VERSION,
        role: role_name.to_string(),
        system_prompt,
        intent: Some(intent),
        target_commit: None,
        diff: None,
        findings: findings
            .iter()
            .map(AgentFindingWire::from_finding)
            .collect(),
        scope: ReviewScope::Full.as_str().to_string(),
    };
    serde_json::to_string(&wire).map_err(|error| CoreError::MalformedAgentInput(error.to_string()))
}

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
    let system_prompt = validate_system_prompt(role, wire.system_prompt)?;
    let findings = wire
        .findings
        .into_iter()
        .map(AgentFindingWire::into_finding)
        .collect::<Result<Vec<_>>>()?;
    let scope = ReviewScope::parse(&wire.scope)?;
    if scope == ReviewScope::Correctif && role != AgentRole::Reviewer {
        return Err(CoreError::MalformedAgentInput(format!(
            "{} input must not carry a \"correctif\" scope (only the reviewer can be scoped)",
            role.as_str()
        )));
    }

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
            if let Some(field) = coder_only_violation(&wire.target_commit, &wire.diff) {
                return Err(CoreError::MalformedAgentInput(format!(
                    "coder input must not carry a {field} (the coder's own worktree is already \
                     checked out at that commit; it runs `git diff` itself)"
                )));
            }
            Ok(AgentInputMessage {
                role,
                system_prompt,
                intent: Some(intent),
                target_commit: None,
                diff: None,
                findings,
                scope,
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
                system_prompt,
                intent: None,
                target_commit: Some(target_commit),
                diff: Some(wire.diff.unwrap_or_default()),
                findings,
                scope,
            })
        }
    }
}

fn coder_only_violation(
    target_commit: &Option<String>,
    diff: &Option<String>,
) -> Option<&'static str> {
    match (target_commit, diff) {
        (Some(_), _) => Some("target_commit"),
        (_, Some(_)) => Some("diff"),
        _ => None,
    }
}

fn validate_system_prompt(role: AgentRole, system_prompt: String) -> Result<String> {
    if system_prompt.trim().is_empty() {
        return Err(CoreError::MalformedAgentInput(format!(
            "{} input system_prompt must not be blank",
            role.as_str()
        )));
    }
    Ok(system_prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM_PROMPT: &str = "You are Warden's agent.";

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
        let message =
            AgentInputMessage::for_coder(SYSTEM_PROMPT, "implement the thing", Vec::new()).unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();
        assert_eq!(decoded, message);
        assert_eq!(decoded.role, AgentRole::Coder);
        assert_eq!(decoded.system_prompt, SYSTEM_PROMPT);
        assert_eq!(decoded.intent.as_deref(), Some("implement the thing"));
        assert!(decoded.target_commit.is_none());
        assert!(decoded.diff.is_none());
        assert!(decoded.findings.is_empty());
    }

    #[test]
    fn coder_input_round_trips_with_the_findings_it_must_fix() {
        let message = AgentInputMessage::for_coder(
            SYSTEM_PROMPT,
            "implement the thing",
            vec![sample_finding()],
        )
        .unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();

        assert_eq!(decoded, message);
        assert_eq!(decoded.role, AgentRole::Coder);
        assert_eq!(decoded.intent.as_deref(), Some("implement the thing"));
        assert_eq!(decoded.findings.len(), 1);
        assert_eq!(decoded.findings[0].source, FindingSource::Ci);
        assert_eq!(decoded.findings[0].description, "build failed");
        assert!(decoded.target_commit.is_none());
        assert!(decoded.diff.is_none());
    }

    #[test]
    fn finding_agent_input_round_trips_through_json_with_findings() {
        let message = AgentInputMessage::for_finding_agent(
            AgentRole::Reviewer,
            SYSTEM_PROMPT,
            "abc123",
            "diff --git a/x b/x\n+added line\n",
            vec![sample_finding()],
        )
        .unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();

        assert_eq!(decoded, message);
        assert_eq!(decoded.role, AgentRole::Reviewer);
        assert_eq!(decoded.system_prompt, SYSTEM_PROMPT);
        assert!(decoded.intent.is_none());
        assert_eq!(decoded.target_commit.as_deref(), Some("abc123"));
        assert_eq!(
            decoded.diff.as_deref(),
            Some("diff --git a/x b/x\n+added line\n")
        );
        assert_eq!(decoded.findings.len(), 1);
        assert_eq!(decoded.findings[0].source, FindingSource::Ci);
        assert_eq!(decoded.scope, ReviewScope::Full);
    }

    #[test]
    fn scoped_review_input_round_trips_through_json_as_correctif_scope() {
        let message = AgentInputMessage::for_scoped_review(
            SYSTEM_PROMPT,
            "def456",
            "diff --git a/x b/x\n+fixed the bug\n",
            vec![sample_finding()],
        )
        .unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();

        assert_eq!(decoded, message);
        assert_eq!(decoded.role, AgentRole::Reviewer);
        assert_eq!(decoded.scope, ReviewScope::Correctif);
        assert_eq!(decoded.target_commit.as_deref(), Some("def456"));
        assert_eq!(
            decoded.diff.as_deref(),
            Some("diff --git a/x b/x\n+fixed the bug\n")
        );
        assert_eq!(decoded.findings.len(), 1);
        assert!(json.contains(r#""scope":"correctif""#));
    }

    #[test]
    fn for_scoped_review_rejects_a_blank_target_commit() {
        assert!(matches!(
            AgentInputMessage::for_scoped_review(SYSTEM_PROMPT, "   ", "diff", vec![]),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn review_scope_round_trips_through_its_string_form() {
        for scope in [ReviewScope::Full, ReviewScope::Correctif] {
            assert_eq!(ReviewScope::parse(scope.as_str()).unwrap(), scope);
        }
        assert!(ReviewScope::parse("ghost").is_err());
    }

    #[test]
    fn tester_input_with_no_prior_findings_round_trips_to_an_empty_list() {
        let message = AgentInputMessage::for_finding_agent(
            AgentRole::Tester,
            SYSTEM_PROMPT,
            "abc123",
            "",
            Vec::new(),
        )
        .unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();
        assert!(decoded.findings.is_empty());
        assert_eq!(decoded.diff.as_deref(), Some(""));
    }

    #[test]
    fn for_finding_agent_rejects_the_coder_role() {
        let result = AgentInputMessage::for_finding_agent(
            AgentRole::Coder,
            SYSTEM_PROMPT,
            "abc123",
            "",
            vec![],
        );
        assert!(matches!(result, Err(CoreError::MalformedAgentInput(_))));
    }

    #[test]
    fn for_coder_rejects_a_blank_intent() {
        assert!(matches!(
            AgentInputMessage::for_coder(SYSTEM_PROMPT, "", Vec::new()),
            Err(CoreError::MalformedAgentInput(_))
        ));
        assert!(matches!(
            AgentInputMessage::for_coder(SYSTEM_PROMPT, "   \n\t ", Vec::new()),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn for_finding_agent_rejects_a_blank_target_commit() {
        assert!(matches!(
            AgentInputMessage::for_finding_agent(
                AgentRole::Reviewer,
                SYSTEM_PROMPT,
                "",
                "",
                vec![]
            ),
            Err(CoreError::MalformedAgentInput(_))
        ));
        assert!(matches!(
            AgentInputMessage::for_finding_agent(
                AgentRole::Tester,
                SYSTEM_PROMPT,
                "   ",
                "",
                vec![]
            ),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn both_constructors_reject_a_blank_system_prompt() {
        assert!(matches!(
            AgentInputMessage::for_coder("  \n\t", "do the thing", Vec::new()),
            Err(CoreError::MalformedAgentInput(_))
        ));
        assert!(matches!(
            AgentInputMessage::for_finding_agent(AgentRole::Reviewer, "", "abc123", "", vec![]),
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
        let json = r#"{"version":99,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_a_version_1_payload() {
        let json = r#"{"version":1,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_a_genuine_version_2_payload_with_no_scope_field() {
        let json = r#"{"version":2,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":null,"diff":null,"findings":[]}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_role() {
        let json = r#"{"version":3,"role":"ghost","system_prompt":"x","intent":"x","target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::UnknownRole(_))
        ));
    }

    #[test]
    fn rejects_a_payload_missing_system_prompt() {
        let json = r#"{"version":3,"role":"coder","intent":"x","target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_a_payload_whose_system_prompt_is_blank() {
        let json = r#"{"version":3,"role":"reviewer","system_prompt":"   ","target_commit":"abc","diff":"","findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_a_coder_payload_missing_intent() {
        let json = r#"{"version":3,"role":"coder","system_prompt":"be a coder","intent":null,"target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_a_coder_payload_whose_intent_is_blank_even_when_it_carries_findings() {
        let json = r#"{"version":3,"role":"coder","system_prompt":"be a coder","intent":"   ","target_commit":null,"diff":null,"findings":[{"source":"reviewer","severity":"blocking","file":null,"description":"x","action":null}],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_a_coder_payload_that_carries_a_target_commit_or_diff() {
        let with_commit = r#"{"version":3,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":"abc123","diff":null,"findings":[],"scope":"full"}"#;
        assert!(
            matches!(
                parse_agent_input_message(with_commit),
                Err(CoreError::MalformedAgentInput(_))
            ),
            "a coder payload with a target_commit must be rejected, not silently stripped: {:?}",
            parse_agent_input_message(with_commit)
        );

        let with_diff = r#"{"version":3,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":null,"diff":"diff --git a/x b/x","findings":[],"scope":"full"}"#;
        assert!(
            matches!(
                parse_agent_input_message(with_diff),
                Err(CoreError::MalformedAgentInput(_))
            ),
            "a coder payload with a diff must be rejected, not silently stripped: {:?}",
            parse_agent_input_message(with_diff)
        );
    }

    #[test]
    fn rejects_a_reviewer_payload_missing_target_commit() {
        let json = r#"{"version":3,"role":"reviewer","system_prompt":"be a reviewer","intent":null,"target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_finding_source_inside_findings() {
        let json = r#"{"version":3,"role":"tester","system_prompt":"be a tester","intent":null,"target_commit":"abc","diff":"","findings":[{"source":"   ","severity":"blocking","file":null,"description":"x","action":null}],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::UnknownFindingSource(_))
        ));
    }

    #[test]
    fn rejects_a_tester_payload_with_a_correctif_scope() {
        let json = r#"{"version":3,"role":"tester","system_prompt":"be a tester","intent":null,"target_commit":"abc","diff":"","findings":[],"scope":"correctif"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_a_coder_payload_with_a_correctif_scope() {
        let json = r#"{"version":3,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":null,"diff":null,"findings":[],"scope":"correctif"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn build_finding_agent_input_json_round_trips_for_a_custom_role() {
        let json = build_finding_agent_input_json(
            "techlead",
            SYSTEM_PROMPT,
            "abc123",
            "diff --git a/x b/x\n+added line\n",
            vec![sample_finding()],
            ReviewScope::Full,
        )
        .unwrap();
        assert!(json.contains(r#""role":"techlead""#));
        assert!(json.contains(r#""version":3"#));

        assert!(matches!(
            parse_agent_input_message(&json),
            Err(CoreError::UnknownRole(_))
        ));
    }

    #[test]
    fn build_finding_agent_input_json_carries_a_correctif_scope_when_asked() {
        let json = build_finding_agent_input_json(
            "techlead",
            SYSTEM_PROMPT,
            "abc123",
            "diff",
            vec![],
            ReviewScope::Correctif,
        )
        .unwrap();
        assert!(json.contains(r#""scope":"correctif""#));
    }

    #[test]
    fn build_finding_agent_input_json_rejects_a_blank_role_name() {
        assert!(matches!(
            build_finding_agent_input_json(
                "   ",
                SYSTEM_PROMPT,
                "abc123",
                "",
                vec![],
                ReviewScope::Full
            ),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn build_finding_agent_input_json_rejects_a_blank_system_prompt() {
        assert!(matches!(
            build_finding_agent_input_json(
                "techlead",
                "  ",
                "abc123",
                "",
                vec![],
                ReviewScope::Full
            ),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn build_finding_agent_input_json_rejects_a_blank_target_commit() {
        assert!(matches!(
            build_finding_agent_input_json(
                "techlead",
                SYSTEM_PROMPT,
                "   ",
                "",
                vec![],
                ReviewScope::Full
            ),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn build_producer_input_json_round_trips_for_a_renamed_producer_role() {
        let json = build_producer_input_json(
            "implementer",
            SYSTEM_PROMPT,
            "implement the thing",
            vec![sample_finding()],
        )
        .unwrap();
        assert!(json.contains(r#""role":"implementer""#));
        assert!(json.contains(r#""intent":"implement the thing""#));
        assert!(json.contains(r#""target_commit":null"#));
        assert!(json.contains(r#""diff":null"#));
    }

    #[test]
    fn build_producer_input_json_rejects_a_blank_role_name() {
        assert!(matches!(
            build_producer_input_json("   ", SYSTEM_PROMPT, "implement it", vec![]),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn build_producer_input_json_rejects_a_blank_system_prompt() {
        assert!(matches!(
            build_producer_input_json("coder", "  ", "implement it", vec![]),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn build_producer_input_json_rejects_a_blank_intent() {
        assert!(matches!(
            build_producer_input_json("coder", SYSTEM_PROMPT, "   ", vec![]),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_scope_string() {
        let json = r#"{"version":3,"role":"reviewer","system_prompt":"be a reviewer","intent":null,"target_commit":"abc","diff":"","findings":[],"scope":"ghost"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }
}
