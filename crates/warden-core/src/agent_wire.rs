use serde::{Deserialize, Serialize};

use crate::convergence::{Finding, FindingSource, Severity};
use crate::error::{CoreError, Result};
use crate::workflow::Role;

pub const AGENT_INPUT_VERSION: u32 = 4;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AgentInputWire {
    version: u32,
    role: String,
    system_prompt: String,
    intent: String,
    current_commit: String,
    diff: String,
    findings: Vec<AgentFindingWire>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInputMessage {
    pub role: Role,
    pub system_prompt: String,
    pub intent: String,
    pub current_commit: String,
    pub diff: String,
    pub findings: Vec<Finding>,
}

impl AgentInputMessage {
    pub fn new(
        role: impl Into<String>,
        system_prompt: impl Into<String>,
        intent: impl Into<String>,
        current_commit: impl Into<String>,
        diff: impl Into<String>,
        findings: Vec<Finding>,
    ) -> Result<Self> {
        let role = Role::new(role.into())
            .map_err(|error| CoreError::MalformedAgentInput(error.to_string()))?;
        let system_prompt = require_non_blank("system_prompt", system_prompt.into())?;
        let intent = require_non_blank("intent", intent.into())?;
        let current_commit = require_non_blank("current_commit", current_commit.into())?;
        Ok(Self {
            role,
            system_prompt,
            intent,
            current_commit,
            diff: diff.into(),
            findings,
        })
    }

    pub fn to_json(&self) -> Result<String> {
        let wire = AgentInputWire {
            version: AGENT_INPUT_VERSION,
            role: self.role.as_str().to_string(),
            system_prompt: self.system_prompt.clone(),
            intent: self.intent.clone(),
            current_commit: self.current_commit.clone(),
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

pub fn build_step_input_json(
    role: &str,
    system_prompt: impl Into<String>,
    intent: impl Into<String>,
    current_commit: impl Into<String>,
    diff: impl Into<String>,
    findings: Vec<Finding>,
) -> Result<String> {
    AgentInputMessage::new(role, system_prompt, intent, current_commit, diff, findings)?.to_json()
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
    let findings = wire
        .findings
        .into_iter()
        .map(AgentFindingWire::into_finding)
        .collect::<Result<Vec<_>>>()?;
    AgentInputMessage::new(
        wire.role,
        wire.system_prompt,
        wire.intent,
        wire.current_commit,
        wire.diff,
        findings,
    )
}

fn require_non_blank(field: &str, value: String) -> Result<String> {
    if value.trim().is_empty() {
        Err(CoreError::MalformedAgentInput(format!(
            "agent input {field} must not be blank"
        )))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_uses_same_payload() {
        let message = AgentInputMessage::new(
            "security",
            "Review security.",
            "ship feature",
            "abc123",
            "diff --git a/a b/a",
            vec![Finding {
                source: FindingSource::Ci,
                severity: Severity::Blocking,
                file: None,
                description: "failed".to_string(),
                action: None,
            }],
        )
        .unwrap();
        assert_eq!(
            parse_agent_input_message(&message.to_json().unwrap()).unwrap(),
            message
        );
    }

    #[test]
    fn required_context_is_never_implicit() {
        assert!(AgentInputMessage::new("step", "prompt", "", "abc", "", vec![]).is_err());
        assert!(AgentInputMessage::new("step", "prompt", "intent", "", "", vec![]).is_err());
        assert!(AgentInputMessage::new("", "prompt", "intent", "abc", "", vec![]).is_err());
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let raw = r#"{"version":3,"role":"step","system_prompt":"p","intent":"i","current_commit":"c","diff":"","findings":[]}"#;
        assert!(parse_agent_input_message(raw).is_err());
    }
}
