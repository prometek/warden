use std::collections::{BTreeMap, HashSet, VecDeque};

use serde::Deserialize;

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Role(String);

impl Role {
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(CoreError::InvalidWorkflow(
                "a step id must not be blank".to_string(),
            ));
        }
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        {
            return Err(CoreError::InvalidWorkflow(format!(
                "step id {name:?} must contain only letters, digits, '.', '_', or '-'"
            )));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Agent,
    Command,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Command => "command",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "agent" => Ok(Self::Agent),
            "command" => Ok(Self::Command),
            other => Err(CoreError::InvalidWorkflow(format!(
                "unknown step type {other:?} (expected \"agent\" or \"command\")"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    Clean,
    Blocking,
    Error,
}

impl StepOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Blocking => "blocking",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepTarget {
    Step(u32),
    Converged,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepTransitions {
    pub clean: StepTarget,
    pub blocking: StepTarget,
    pub error: StepTarget,
}

impl StepTransitions {
    pub fn for_outcome(self, outcome: StepOutcome) -> StepTarget {
        match outcome {
            StepOutcome::Clean => self.clean,
            StepOutcome::Blocking => self.blocking,
            StepOutcome::Error => self.error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowStep {
    pub role: Role,
    pub kind: StepKind,
    pub agent: Option<String>,
    pub run: Option<String>,
    pub transitions: StepTransitions,
    pub max_cycles: Option<u32>,
    pub captures_evidence: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    pub name: String,
    pub entry_step: u32,
    pub steps: Vec<WorkflowStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowWire {
    name: String,
    entry: String,
    steps: BTreeMap<String, WorkflowStepWire>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowStepWire {
    #[serde(rename = "type")]
    kind: String,
    agent: Option<String>,
    run: Option<String>,
    on_clean: String,
    on_blocking: String,
    on_error: String,
    max_cycles: Option<u32>,
    #[serde(default)]
    evidence: bool,
}

impl Workflow {
    pub fn parse_yaml(raw: &str) -> Result<Self> {
        let wire: WorkflowWire = serde_yaml::from_str(raw)
            .map_err(|error| CoreError::InvalidWorkflow(format!("invalid YAML: {error}")))?;
        if wire.name.trim().is_empty() {
            return Err(CoreError::InvalidWorkflow(
                "workflow name must not be blank".to_string(),
            ));
        }
        if wire.steps.is_empty() {
            return Err(CoreError::InvalidWorkflow(
                "workflow must declare at least one step".to_string(),
            ));
        }

        let ids = wire
            .steps
            .keys()
            .map(|id| Role::new(id.clone()))
            .collect::<Result<Vec<_>>>()?;
        for reserved in ["converged", "failed", "ci", "warden"] {
            if ids.iter().any(|id| id.as_str() == reserved) {
                return Err(CoreError::InvalidWorkflow(format!(
                    "step id {reserved:?} is reserved"
                )));
            }
        }
        let entry_step = step_index(&ids, &wire.entry).ok_or_else(|| {
            CoreError::InvalidWorkflow(format!(
                "entry {:?} does not name a declared step",
                wire.entry
            ))
        })?;

        let mut steps = Vec::with_capacity(wire.steps.len());
        for (id, step) in wire.steps {
            let role = Role::new(id)?;
            let kind = StepKind::parse(&step.kind)
                .map_err(|error| CoreError::InvalidWorkflow(format!("step {role:?}: {error}")))?;
            validate_execution_fields(&role, kind, &step)?;
            if step.max_cycles == Some(0) {
                return Err(CoreError::InvalidWorkflow(format!(
                    "step {role:?}: max_cycles must be at least 1"
                )));
            }
            let transitions = StepTransitions {
                clean: parse_target(&ids, &role, "on_clean", &step.on_clean)?,
                blocking: parse_target(&ids, &role, "on_blocking", &step.on_blocking)?,
                error: parse_target(&ids, &role, "on_error", &step.on_error)?,
            };
            steps.push(WorkflowStep {
                role,
                kind,
                agent: step.agent,
                run: step.run,
                transitions,
                max_cycles: step.max_cycles,
                captures_evidence: step.evidence,
            });
        }

        let workflow = Self {
            name: wire.name,
            entry_step,
            steps,
        };
        workflow.validate_graph()?;
        Ok(workflow)
    }

    pub fn entry(&self) -> u32 {
        self.entry_step
    }

    pub fn step_index(&self, id: &str) -> Option<u32> {
        self.steps
            .iter()
            .position(|step| step.role.as_str() == id)
            .and_then(|index| u32::try_from(index).ok())
    }

    pub fn target_for(&self, step_index: u32, outcome: StepOutcome) -> StepTarget {
        self.steps[step_index as usize]
            .transitions
            .for_outcome(outcome)
    }

    fn validate_graph(&self) -> Result<()> {
        let mut reachable = HashSet::new();
        let mut queue = VecDeque::from([self.entry_step]);
        let mut converged_reachable = false;
        while let Some(index) = queue.pop_front() {
            if !reachable.insert(index) {
                continue;
            }
            let transitions = self.steps[index as usize].transitions;
            for target in [transitions.clean, transitions.blocking, transitions.error] {
                match target {
                    StepTarget::Step(next) => queue.push_back(next),
                    StepTarget::Converged => converged_reachable = true,
                    StepTarget::Failed => {}
                }
            }
        }
        if reachable.len() != self.steps.len() {
            let unreachable = self
                .steps
                .iter()
                .enumerate()
                .filter(|(index, _)| !reachable.contains(&(*index as u32)))
                .map(|(_, step)| step.role.as_str())
                .collect::<Vec<_>>();
            return Err(CoreError::InvalidWorkflow(format!(
                "workflow contains unreachable steps: {}",
                unreachable.join(", ")
            )));
        }
        if !converged_reachable {
            return Err(CoreError::InvalidWorkflow(
                "workflow has no reachable transition to \"converged\"".to_string(),
            ));
        }
        Ok(())
    }
}

fn step_index(ids: &[Role], target: &str) -> Option<u32> {
    ids.iter()
        .position(|id| id.as_str() == target)
        .and_then(|index| u32::try_from(index).ok())
}

fn parse_target(ids: &[Role], role: &Role, field: &str, raw: &str) -> Result<StepTarget> {
    match raw {
        "converged" => Ok(StepTarget::Converged),
        "failed" => Ok(StepTarget::Failed),
        other => step_index(ids, other).map(StepTarget::Step).ok_or_else(|| {
            CoreError::InvalidWorkflow(format!(
                "step {role:?}: {field} target {other:?} is neither a declared step nor a terminal"
            ))
        }),
    }
}

fn validate_execution_fields(role: &Role, kind: StepKind, step: &WorkflowStepWire) -> Result<()> {
    match kind {
        StepKind::Agent => {
            let agent = step.agent.as_deref().unwrap_or("");
            Role::new(agent.to_string()).map_err(|_| {
                CoreError::InvalidWorkflow(format!(
                    "step {role:?}: type: agent requires a valid non-blank agent name"
                ))
            })?;
            if step.run.is_some() {
                return Err(CoreError::InvalidWorkflow(format!(
                    "step {role:?}: run is only valid for type: command"
                )));
            }
        }
        StepKind::Command => {
            if step.run.as_deref().is_none_or(|run| run.trim().is_empty()) {
                return Err(CoreError::InvalidWorkflow(format!(
                    "step {role:?}: type: command requires a non-blank run value"
                )));
            }
            if step.agent.is_some() {
                return Err(CoreError::InvalidWorkflow(format!(
                    "step {role:?}: agent is only valid for type: agent"
                )));
            }
            if step.evidence {
                return Err(CoreError::InvalidWorkflow(format!(
                    "step {role:?}: type: command cannot capture agent evidence"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
name: quality-loop
entry: implementation
steps:
  implementation:
    type: agent
    agent: implementer
    on_clean: review
    on_blocking: implementation
    on_error: failed
  lint:
    type: command
    run: cargo fmt --check
    on_clean: verification
    on_blocking: implementation
    on_error: failed
  review:
    type: agent
    agent: reviewer
    on_clean: lint
    on_blocking: implementation
    on_error: failed
    max_cycles: 3
  verification:
    type: agent
    agent: verifier
    on_clean: converged
    on_blocking: implementation
    on_error: failed
    evidence: true
"#;

    #[test]
    fn parses_graph_without_reserved_role_semantics() {
        let workflow = Workflow::parse_yaml(VALID).unwrap();
        assert_eq!(
            workflow.steps[workflow.entry() as usize].role.as_str(),
            "implementation"
        );
        let review = workflow.step_index("review").unwrap();
        assert_eq!(workflow.steps[review as usize].max_cycles, Some(3));
        assert_eq!(
            workflow.target_for(review, StepOutcome::Blocking),
            StepTarget::Step(workflow.entry())
        );
        let lint = workflow.step_index("lint").unwrap();
        assert_eq!(workflow.steps[lint as usize].kind, StepKind::Command);
    }

    #[test]
    fn entry_may_be_any_declared_step() {
        let workflow =
            Workflow::parse_yaml(&VALID.replace("entry: implementation", "entry: review")).unwrap();
        assert_eq!(
            workflow.steps[workflow.entry() as usize].role.as_str(),
            "review"
        );
    }

    #[test]
    fn rejects_unknown_transition_target() {
        let raw = VALID.replace("on_clean: review", "on_clean: nowhere");
        assert!(Workflow::parse_yaml(&raw)
            .unwrap_err()
            .to_string()
            .contains("nowhere"));
    }

    #[test]
    fn rejects_unreachable_step() {
        let raw = VALID.replace("on_clean: lint", "on_clean: verification");
        assert!(Workflow::parse_yaml(&raw)
            .unwrap_err()
            .to_string()
            .contains("unreachable steps: lint"));
    }

    #[test]
    fn rejects_graph_without_converged_transition() {
        let raw = VALID.replace("on_clean: converged", "on_clean: implementation");
        assert!(Workflow::parse_yaml(&raw)
            .unwrap_err()
            .to_string()
            .contains("no reachable transition"));
    }

    #[test]
    fn rejects_implicit_or_unknown_step_type() {
        let missing = VALID.replace(
            "    type: agent\n    agent: implementer",
            "    agent: implementer",
        );
        assert!(Workflow::parse_yaml(&missing).is_err());
        let unknown = VALID.replace("type: command", "type: hook");
        assert!(Workflow::parse_yaml(&unknown)
            .unwrap_err()
            .to_string()
            .contains("hook"));
    }

    #[test]
    fn rejects_invalid_execution_fields() {
        let command_with_agent = VALID.replace(
            "    type: command\n    run: cargo fmt --check",
            "    type: command\n    run: cargo fmt --check\n    agent: lint-agent",
        );
        assert!(Workflow::parse_yaml(&command_with_agent).is_err());
        let agent_with_run = VALID.replace(
            "    type: agent\n    agent: reviewer",
            "    type: agent\n    agent: reviewer\n    run: cargo test",
        );
        assert!(Workflow::parse_yaml(&agent_with_run).is_err());
    }

    #[test]
    fn rejects_zero_max_cycles() {
        let raw = VALID.replace("max_cycles: 3", "max_cycles: 0");
        assert!(Workflow::parse_yaml(&raw).is_err());
    }

    #[test]
    fn rejects_reserved_and_path_like_ids() {
        let reserved = VALID.replace("  review:\n", "  warden:\n");
        assert!(Workflow::parse_yaml(&reserved).is_err());
        let path = VALID.replace("  review:\n", "  ../review:\n");
        assert!(Workflow::parse_yaml(&path).is_err());
    }

    /// A published workflow graph (issue #107) encodes each transition as either a step id or the
    /// bare strings `"converged"` / `"failed"`. That encoding is only unambiguous because no step
    /// may ever be *named* after a terminal -- pin it here, at the parser that guarantees it.
    #[test]
    fn a_step_may_never_be_named_after_a_terminal_transition_target() {
        for terminal in ["converged", "failed"] {
            let raw = VALID.replace("  review:\n", &format!("  {terminal}:\n"));
            let error = Workflow::parse_yaml(&raw)
                .expect_err("a step named after a terminal must be rejected")
                .to_string();
            assert!(error.contains(terminal), "{error}");
            assert!(error.contains("reserved"), "{error}");
        }
    }

    #[test]
    fn step_transitions_cover_every_outcome() {
        let workflow = Workflow::parse_yaml(VALID).unwrap();
        let verification = workflow.step_index("verification").unwrap();
        assert_eq!(
            workflow.target_for(verification, StepOutcome::Clean),
            StepTarget::Converged
        );
        assert_eq!(
            workflow.target_for(verification, StepOutcome::Error),
            StepTarget::Failed
        );
    }
}
