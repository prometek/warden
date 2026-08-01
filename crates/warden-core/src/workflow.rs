use serde::Deserialize;

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Role(String);

impl Role {
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(CoreError::InvalidWorkflow(
                "a step's role must not be blank".to_string(),
            ));
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

/// How a step's findings gate the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// No gate at all: the step runs once per cycle and never reboucles the pipeline back to the
    /// first step, whatever it reports.
    PassThrough,
    LoopUntilClean,
    ScopedReReview,
}

impl Gate {
    pub fn as_str(self) -> &'static str {
        match self {
            Gate::PassThrough => "pass-through",
            Gate::LoopUntilClean => "loop-until-clean",
            Gate::ScopedReReview => "scoped-re-review",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "pass-through" => Ok(Gate::PassThrough),
            "loop-until-clean" => Ok(Gate::LoopUntilClean),
            "scoped-re-review" => Ok(Gate::ScopedReReview),
            other => Err(CoreError::InvalidWorkflow(format!(
                "unknown gate {other:?} (expected \"loop-until-clean\" or \"scoped-re-review\", \
                 or omit the key for a plain pass-through)"
            ))),
        }
    }
}

/// A step's execution mechanism: whether the step spawns an **agent** subprocess (an LLM's own
/// judgement) or runs a deterministic **hook** command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Agent,
    Hook,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Agent => "agent",
            StepKind::Hook => "hook",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "agent" => Ok(StepKind::Agent),
            "hook" => Ok(StepKind::Hook),
            other => Err(CoreError::InvalidWorkflow(format!(
                "unknown type {other:?} (expected \"agent\" or \"hook\", or omit the key for \
                 \"agent\")"
            ))),
        }
    }
}

/// Which run-level cycle budget a gated step's blocking findings are charged against, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepBudget {
    Review,
    /// `max_test_cycles` -- charged unconditionally, once per invocation (this step runs, and its
    /// counter advances, every time the pipeline reaches it).
    Test,
    Extra,
    /// this step's own, independent cycle budget (`max_cycles: N` in `workflow.yaml`), instead of
    /// one of the three run-level buckets above.
    Own(u32),
}

impl StepBudget {
    /// A label for this budget kind -- **not** a round-trippable wire form for [`StepBudget::Own`]
    /// (its `u32` payload isn't representable in a `&'static str`).
    pub fn as_str(self) -> &'static str {
        match self {
            StepBudget::Review => "review",
            StepBudget::Test => "test",
            StepBudget::Extra => "extra",
            StepBudget::Own(_) => "own",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "review" => Ok(StepBudget::Review),
            "test" => Ok(StepBudget::Test),
            "extra" => Ok(StepBudget::Extra),
            other => Err(CoreError::InvalidWorkflow(format!(
                "unknown budget {other:?} (expected \"review\", \"test\", \"extra\", or omit the \
                 key for \"extra\"; for this step's own independent cycle budget, use \
                 \"max_cycles: N\" instead of \"budget\")"
            ))),
        }
    }
}

/// One step of a [`Workflow`]: a [`Role`] resolved to an `agent`, gated by an optional [`Gate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowStep {
    pub role: Role,
    pub kind: StepKind,
    pub agent: Option<String>,
    /// the shell command a `type: hook` step runs (`sh -c "<run>"`, mirroring `crate::hook`'s own
    /// `CommandHook`).
    pub run: Option<String>,
    pub gate: Gate,
    pub budget: Option<StepBudget>,
    pub captures_evidence: bool,
}

/// A user-definable pipeline: an ordered, non-empty list of [`WorkflowStep`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    pub name: String,
    pub steps: Vec<WorkflowStep>,
}

/// Wire shape of one `workflow.yaml` step -- `gate` absent means "plain pass-through" (never
/// "reject", never "assume loop-until-clean".
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowStepWire {
    role: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    agent: Option<String>,
    run: Option<String>,
    gate: Option<String>,
    budget: Option<String>,
    max_cycles: Option<u32>,
    #[serde(default)]
    evidence: bool,
}

/// Wire shape of `.warden/workflow.yaml` itself.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowWire {
    name: String,
    steps: Vec<WorkflowStepWire>,
}

fn reject_path_like_value(step_index: usize, field: &'static str, value: &str) -> Result<()> {
    let has_separator = value.contains('/') || value.contains('\\');
    let has_dotdot_component = value.split(['/', '\\']).any(|part| part == "..");
    if has_separator || has_dotdot_component {
        return Err(CoreError::InvalidWorkflow(format!(
            "step {step_index}: {field} {value:?} must not contain a path separator (\"/\" or \
             \"\\\") or a \"..\" component"
        )));
    }
    Ok(())
}

impl Workflow {
    pub fn builtin_default() -> Self {
        // Constructed from already-valid literals -- `expect` here is an invariant on this crate's
        // own hardcoded default, never on user-controlled input (`parse_yaml`'s job).
        Self {
            name: "default".to_string(),
            steps: vec![
                WorkflowStep {
                    role: Role::new("coder").expect("literal role name is never blank"),
                    kind: StepKind::Agent,
                    agent: Some("coder".to_string()),
                    run: None,
                    gate: Gate::PassThrough,
                    budget: None,
                    captures_evidence: false,
                },
                WorkflowStep {
                    role: Role::new("reviewer").expect("literal role name is never blank"),
                    kind: StepKind::Agent,
                    agent: Some("code-reviewer".to_string()),
                    run: None,
                    gate: Gate::LoopUntilClean,
                    budget: Some(StepBudget::Review),
                    captures_evidence: false,
                },
                WorkflowStep {
                    role: Role::new("tester").expect("literal role name is never blank"),
                    kind: StepKind::Agent,
                    agent: Some("test-runner".to_string()),
                    run: None,
                    gate: Gate::LoopUntilClean,
                    budget: Some(StepBudget::Test),
                    captures_evidence: true,
                },
            ],
        }
    }

    /// Parses and validates a `.warden/workflow.yaml` document.
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

        let mut steps = Vec::with_capacity(wire.steps.len());
        let mut seen_roles = std::collections::HashSet::new();
        let mut review_budget_step: Option<usize> = None;
        let mut test_budget_step: Option<usize> = None;
        let mut evidence_capture_step: Option<usize> = None;
        for (index, step) in wire.steps.into_iter().enumerate() {
            reject_path_like_value(index, "role", &step.role)?;
            if crate::convergence::FindingSource::RESERVED_ROLE_NAMES.contains(&step.role.as_str())
            {
                return Err(CoreError::InvalidWorkflow(format!(
                    "step {index}: role {:?} is reserved (findings from a \"ci\" or \"warden\" \
                     step could never be told apart from `FindingSource::Ci`/`FindingSource::Warden`, \
                     which are not role findings) -- pick a different role name",
                    step.role
                )));
            }
            let role = Role::new(step.role)
                .map_err(|error| CoreError::InvalidWorkflow(format!("step {index}: {error}")))?;

            let kind = match &step.kind {
                None => StepKind::Agent,
                Some(raw_kind) if raw_kind == "policy" => {
                    return Err(CoreError::InvalidWorkflow(format!(
                        "step {index} (role {role:?}): type: policy is not supported yet (see \
                         issue #51) -- use type: agent or type: hook"
                    )));
                }
                Some(raw_kind) => StepKind::parse(raw_kind).map_err(|error| {
                    CoreError::InvalidWorkflow(format!("step {index} (role {role:?}): {error}"))
                })?,
            };
            if index == 0 && kind != StepKind::Agent {
                return Err(CoreError::InvalidWorkflow(format!(
                    "the first step (role {role:?}) is the pipeline's producer and must be type: \
                     agent -- it authors this cycle's commit, which a type: {} step cannot do",
                    kind.as_str()
                )));
            }
            match kind {
                StepKind::Agent => {
                    let agent_name = step.agent.as_deref().unwrap_or("");
                    if agent_name.trim().is_empty() {
                        return Err(CoreError::InvalidWorkflow(format!(
                            "step {index} (role {role:?}): type: agent requires a non-blank \
                             \"agent\" key naming the agent definition to resolve"
                        )));
                    }
                    reject_path_like_value(index, "agent", agent_name)?;
                    if step.run.is_some() {
                        return Err(CoreError::InvalidWorkflow(format!(
                            "step {index} (role {role:?}): \"run\" is only valid for type: hook \
                             -- this step is type: agent (the default)"
                        )));
                    }
                }
                StepKind::Hook => {
                    let run_command = step.run.as_deref().unwrap_or("");
                    if run_command.trim().is_empty() {
                        return Err(CoreError::InvalidWorkflow(format!(
                            "step {index} (role {role:?}): type: hook requires a non-blank \
                             \"run\" key naming the shell command to execute"
                        )));
                    }
                    if step.agent.is_some() {
                        return Err(CoreError::InvalidWorkflow(format!(
                            "step {index} (role {role:?}): \"agent\" is only valid for type: \
                             agent -- a type: hook step has no agent definition to resolve"
                        )));
                    }
                    if step.evidence {
                        return Err(CoreError::InvalidWorkflow(format!(
                            "step {index} (role {role:?}) is type: hook and cannot capture \
                             evidence -- evidence capture records an agent's command session, \
                             which this step has none of"
                        )));
                    }
                }
            }

            if !seen_roles.insert(role.as_str().to_string()) {
                return Err(CoreError::InvalidWorkflow(format!(
                    "duplicate role {role:?} at step {index} -- every step must have a unique role"
                )));
            }
            let gate = match step.gate {
                Some(raw_gate) => Gate::parse(&raw_gate).map_err(|_| {
                    CoreError::InvalidWorkflow(format!(
                        "step {index} (role {role:?}): unknown gate {raw_gate:?} (expected \
                         \"loop-until-clean\" or \"scoped-re-review\", or omit the key for a \
                         plain pass-through)"
                    ))
                })?,
                None => Gate::PassThrough,
            };
            if index == 0 && gate != Gate::PassThrough {
                return Err(CoreError::InvalidWorkflow(format!(
                    "the first step (role {role:?}) is the pipeline's producer and must be a \
                     plain pass-through (no \"gate\" key) -- only later steps may gate the \
                     pipeline"
                )));
            }

            if index == 0 && step.evidence {
                return Err(CoreError::InvalidWorkflow(format!(
                    "the first step (role {role:?}) is the pipeline's producer and cannot \
                     capture evidence -- remove its \"evidence\" key"
                )));
            }

            let budget = if index == 0 {
                if step.budget.is_some() {
                    return Err(CoreError::InvalidWorkflow(format!(
                        "the first step (role {role:?}) is the pipeline's producer and has no \
                         cycle budget of its own -- remove its \"budget\" key"
                    )));
                }
                if step.max_cycles.is_some() {
                    return Err(CoreError::InvalidWorkflow(format!(
                        "the first step (role {role:?}) is the pipeline's producer and has no \
                         cycle budget of its own -- remove its \"max_cycles\" key"
                    )));
                }
                None
            } else {
                let budget = match (&step.budget, step.max_cycles) {
                    (Some(_), Some(_)) => {
                        return Err(CoreError::InvalidWorkflow(format!(
                            "step {index} (role {role:?}) declares both \"budget\" and \
                             \"max_cycles\" -- pick one: a named run-level bucket via \"budget\", \
                             or this step's own cycle budget via \"max_cycles\""
                        )));
                    }
                    (None, Some(max_cycles)) => {
                        if max_cycles == 0 {
                            return Err(CoreError::InvalidWorkflow(format!(
                                "step {index} (role {role:?}): max_cycles must be at least 1, \
                                 got 0"
                            )));
                        }
                        StepBudget::Own(max_cycles)
                    }
                    (Some(raw_budget), None) => StepBudget::parse(raw_budget).map_err(|_| {
                        CoreError::InvalidWorkflow(format!(
                            "step {index} (role {role:?}): unknown budget {raw_budget:?} \
                             (expected \"review\", \"test\", \"extra\", or omit the key for \
                             \"extra\"; for this step's own independent cycle budget, use \
                             \"max_cycles: N\" instead of \"budget\")"
                        ))
                    })?,
                    (None, None) => StepBudget::Extra,
                };
                let duplicate_of = match budget {
                    StepBudget::Review => &mut review_budget_step,
                    StepBudget::Test => &mut test_budget_step,
                    // `Extra` is the shared bucket -- no single-claimant invariant applies, so
                    // there's nothing to check.
                    StepBudget::Extra | StepBudget::Own(_) => &mut None,
                };
                if let Some(prior_index) = *duplicate_of {
                    return Err(CoreError::InvalidWorkflow(format!(
                        "step {index} (role {role:?}) claims budget \"{}\", already claimed by \
                         step {prior_index} -- only one step may claim each of \"review\"/\"test\"",
                        budget.as_str()
                    )));
                }
                *duplicate_of = Some(index);
                Some(budget)
            };

            if step.evidence {
                if let Some(prior_index) = evidence_capture_step {
                    return Err(CoreError::InvalidWorkflow(format!(
                        "step {index} (role {role:?}) sets \"evidence: true\", already set by \
                         step {prior_index} -- only one step may capture evidence"
                    )));
                }
                evidence_capture_step = Some(index);
            }

            steps.push(WorkflowStep {
                role,
                kind,
                agent: step.agent,
                run: step.run,
                gate,
                budget,
                captures_evidence: step.evidence,
            });
        }

        Ok(Self {
            name: wire.name,
            steps,
        })
    }

    /// The step this pipeline reboucles to when a later step's gate finds a blocking problem --
    /// always the first one.
    pub fn producer_role(&self) -> &Role {
        &self.steps[0].role
    }

    /// `true` when `step_index` is this workflow's last step.
    pub fn is_last_step(&self, step_index: u32) -> bool {
        step_index as usize == self.steps.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_YAML: &str = r#"
name: default
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: code-reviewer
    gate: loop-until-clean
    budget: review
  - role: tester
    agent: test-runner
    gate: loop-until-clean
    budget: test
    evidence: true
"#;

    #[test]
    fn parsing_the_documented_default_shape_matches_builtin_default() {
        assert_eq!(
            Workflow::parse_yaml(DEFAULT_YAML).unwrap(),
            Workflow::builtin_default()
        );
    }

    #[test]
    fn builtin_default_has_the_pre_issue_73_three_step_shape() {
        let workflow = Workflow::builtin_default();
        assert_eq!(workflow.steps.len(), 3);
        assert_eq!(workflow.steps[0].role.as_str(), "coder");
        assert_eq!(workflow.steps[0].gate, Gate::PassThrough);
        assert_eq!(workflow.steps[0].budget, None);
        assert!(!workflow.steps[0].captures_evidence);
        assert_eq!(workflow.steps[1].role.as_str(), "reviewer");
        assert_eq!(workflow.steps[1].gate, Gate::LoopUntilClean);
        assert_eq!(workflow.steps[1].budget, Some(StepBudget::Review));
        assert!(!workflow.steps[1].captures_evidence);
        assert_eq!(workflow.steps[2].role.as_str(), "tester");
        assert_eq!(workflow.steps[2].gate, Gate::LoopUntilClean);
        assert_eq!(workflow.steps[2].budget, Some(StepBudget::Test));
        assert!(workflow.steps[2].captures_evidence);
    }

    #[test]
    fn a_custom_workflow_can_append_a_new_role_after_the_default_pipeline() {
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
        assert_eq!(workflow.steps.len(), 4);
        assert_eq!(workflow.steps[3].role.as_str(), "techlead");
        assert_eq!(workflow.steps[3].gate, Gate::LoopUntilClean);
        assert!(workflow.is_last_step(3));
        assert!(!workflow.is_last_step(1));
    }

    #[test]
    fn a_step_with_no_gate_key_defaults_to_pass_through() {
        let yaml = r#"
name: minimal
steps:
  - role: coder
    agent: coder
  - role: notifier
    agent: notifier
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();
        assert_eq!(workflow.steps[1].gate, Gate::PassThrough);
    }

    #[test]
    fn rejects_malformed_yaml() {
        assert!(matches!(
            Workflow::parse_yaml("not: valid: yaml: at: all: ["),
            Err(CoreError::InvalidWorkflow(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_top_level_key() {
        let yaml = "name: x\nsteps: []\nextra: true\n";
        assert!(matches!(
            Workflow::parse_yaml(yaml),
            Err(CoreError::InvalidWorkflow(_))
        ));
    }

    #[test]
    fn rejects_an_empty_steps_list() {
        let yaml = "name: empty\nsteps: []\n";
        assert!(matches!(
            Workflow::parse_yaml(yaml),
            Err(CoreError::InvalidWorkflow(_))
        ));
    }

    #[test]
    fn rejects_a_blank_role() {
        let yaml = "name: x\nsteps:\n  - role: \"  \"\n    agent: coder\n";
        assert!(matches!(
            Workflow::parse_yaml(yaml),
            Err(CoreError::InvalidWorkflow(_))
        ));
    }

    #[test]
    fn rejects_a_blank_agent() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: \"  \"\n";
        assert!(matches!(
            Workflow::parse_yaml(yaml),
            Err(CoreError::InvalidWorkflow(_))
        ));
    }

    #[test]
    fn rejects_an_agent_name_containing_a_path_traversal_component() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: techlead
    agent: ../../../etc/os-release
    gate: loop-until-clean
"#;
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("path separator"), "{error}");
    }

    #[test]
    fn rejects_an_agent_name_that_is_an_absolute_path() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: /etc/passwd\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("path separator"), "{error}");
    }

    #[test]
    fn rejects_an_agent_name_containing_a_backslash_path_traversal_component() {
        let yaml = r#"name: x
steps:
  - role: coder
    agent: coder
  - role: techlead
    agent: "..\\..\\windows\\system32\\drivers\\etc\\hosts"
    gate: loop-until-clean
"#;
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
    }

    #[test]
    fn rejects_a_role_name_containing_a_path_traversal_component() {
        let yaml = "name: x\nsteps:\n  - role: ../coder\n    agent: coder\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("path separator"), "{error}");
    }

    #[test]
    fn rejects_a_bare_dot_dot_agent_name_with_no_separator() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: \"..\"\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains(".."), "{error}");
    }

    #[test]
    fn accepts_ordinary_hyphenated_role_and_agent_names() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: code-reviewer
    gate: loop-until-clean
  - role: techlead
    agent: tech-lead-v2
    gate: loop-until-clean
"#;
        assert!(Workflow::parse_yaml(yaml).is_ok());
    }

    #[test]
    fn rejects_a_reserved_role_name() {
        for reserved in crate::convergence::FindingSource::RESERVED_ROLE_NAMES {
            let yaml = format!(
                "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: {reserved}\n    \
                 agent: {reserved}\n    gate: loop-until-clean\n"
            );
            let error = Workflow::parse_yaml(&yaml).unwrap_err();
            assert!(matches!(error, CoreError::InvalidWorkflow(_)));
            assert!(error.to_string().contains("reserved"), "{error}");
        }
    }

    #[test]
    fn rejects_a_duplicate_role() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: coder
    agent: another-coder
"#;
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("duplicate role"));
    }

    #[test]
    fn rejects_an_unknown_gate() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: reviewer
    gate: whenever-it-feels-like-it
"#;
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("unknown gate"));
    }

    #[test]
    fn rejects_a_first_step_that_declares_a_gate() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
    gate: loop-until-clean
"#;
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("producer"));
    }

    #[test]
    fn gate_round_trips_through_its_string_form() {
        for gate in [
            Gate::PassThrough,
            Gate::LoopUntilClean,
            Gate::ScopedReReview,
        ] {
            assert_eq!(Gate::parse(gate.as_str()).unwrap(), gate);
        }
        assert!(Gate::parse("ghost").is_err());
    }

    #[test]
    fn a_step_can_declare_the_scoped_re_review_gate() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: techlead
    agent: techlead
    gate: scoped-re-review
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();
        assert_eq!(workflow.steps[1].gate, Gate::ScopedReReview);
    }

    #[test]
    fn unknown_gate_error_names_both_accepted_values() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: reviewer\n    \
                     agent: reviewer\n    gate: whenever-it-feels-like-it\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("loop-until-clean"), "{message}");
        assert!(message.contains("scoped-re-review"), "{message}");
        assert!(
            message.contains("\"whenever-it-feels-like-it\""),
            "{message}"
        );
    }

    #[test]
    fn role_rejects_a_blank_name() {
        assert!(Role::new("").is_err());
        assert!(Role::new("   ").is_err());
        assert!(Role::new("techlead").is_ok());
    }

    #[test]
    fn producer_role_is_always_the_first_step() {
        let workflow = Workflow::builtin_default();
        assert_eq!(workflow.producer_role().as_str(), "coder");
    }

    #[test]
    fn a_gated_step_with_no_budget_key_defaults_to_extra() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: techlead
    agent: techlead
    gate: loop-until-clean
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();
        assert_eq!(workflow.steps[1].budget, Some(StepBudget::Extra));
    }

    #[test]
    fn rejects_a_budget_declared_on_the_first_step() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n    budget: extra\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("producer"), "{error}");
    }

    #[test]
    fn rejects_an_evidence_flag_declared_on_the_first_step() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n    evidence: true\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("producer"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_budget() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: reviewer\n    \
                     agent: reviewer\n    gate: loop-until-clean\n    budget: whenever\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        let message = error.to_string();
        assert!(message.contains("unknown budget"), "{message}");
        assert!(message.contains("max_cycles"), "{message}");
    }

    #[test]
    fn rejects_two_steps_claiming_the_same_review_or_test_budget() {
        for budget in ["review", "test"] {
            let yaml = format!(
                "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: one\n    agent: \
                 one\n    gate: loop-until-clean\n    budget: {budget}\n  - role: two\n    \
                 agent: two\n    gate: loop-until-clean\n    budget: {budget}\n"
            );
            let error = Workflow::parse_yaml(&yaml).unwrap_err();
            assert!(matches!(error, CoreError::InvalidWorkflow(_)));
            assert!(error.to_string().contains("already claimed"), "{error}");
        }
    }

    #[test]
    fn multiple_steps_may_share_the_extra_budget() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: one
    agent: one
    gate: loop-until-clean
    budget: extra
  - role: two
    agent: two
    gate: loop-until-clean
    budget: extra
"#;
        assert!(Workflow::parse_yaml(yaml).is_ok());
    }

    #[test]
    fn a_step_can_declare_its_own_max_cycles_budget() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: techlead
    agent: techlead
    gate: loop-until-clean
    max_cycles: 7
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();
        assert_eq!(workflow.steps[1].budget, Some(StepBudget::Own(7)));
    }

    #[test]
    fn two_steps_may_each_declare_their_own_independent_max_cycles() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: one
    agent: one
    gate: loop-until-clean
    max_cycles: 2
  - role: two
    agent: two
    gate: loop-until-clean
    max_cycles: 9
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();
        assert_eq!(workflow.steps[1].budget, Some(StepBudget::Own(2)));
        assert_eq!(workflow.steps[2].budget, Some(StepBudget::Own(9)));
    }

    #[test]
    fn a_step_can_combine_the_scoped_re_review_gate_with_its_own_max_cycles() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: techlead
    agent: techlead
    gate: scoped-re-review
    max_cycles: 4
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();
        assert_eq!(workflow.steps[1].gate, Gate::ScopedReReview);
        assert_eq!(workflow.steps[1].budget, Some(StepBudget::Own(4)));
    }

    #[test]
    fn a_workflow_combining_scoped_re_review_and_max_cycles_round_trips_through_the_real_parser() {
        let yaml = r#"
name: with-scoped-and-own-budget
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: code-reviewer
    gate: loop-until-clean
    budget: review
  - role: techlead
    agent: tech-lead-v2
    gate: scoped-re-review
    max_cycles: 3
    evidence: true
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();
        assert_eq!(workflow.name, "with-scoped-and-own-budget");
        assert_eq!(workflow.steps.len(), 3);

        assert_eq!(workflow.steps[0].role.as_str(), "coder");
        assert_eq!(workflow.steps[0].gate, Gate::PassThrough);
        assert_eq!(workflow.steps[0].budget, None);
        assert!(!workflow.steps[0].captures_evidence);

        assert_eq!(workflow.steps[1].role.as_str(), "reviewer");
        assert_eq!(workflow.steps[1].agent.as_deref(), Some("code-reviewer"));
        assert_eq!(workflow.steps[1].gate, Gate::LoopUntilClean);
        assert_eq!(workflow.steps[1].budget, Some(StepBudget::Review));
        assert!(!workflow.steps[1].captures_evidence);

        assert_eq!(workflow.steps[2].role.as_str(), "techlead");
        assert_eq!(workflow.steps[2].agent.as_deref(), Some("tech-lead-v2"));
        assert_eq!(workflow.steps[2].gate, Gate::ScopedReReview);
        assert_eq!(workflow.steps[2].budget, Some(StepBudget::Own(3)));
        assert!(workflow.steps[2].captures_evidence);
        assert!(workflow.is_last_step(2));
    }

    #[test]
    fn rejects_a_step_declaring_both_budget_and_max_cycles() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: reviewer\n    \
                     agent: reviewer\n    gate: loop-until-clean\n    budget: review\n    \
                     max_cycles: 3\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(
            error
                .to_string()
                .contains("both \"budget\" and \"max_cycles\""),
            "{error}"
        );
    }

    #[test]
    fn rejects_a_zero_max_cycles() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: reviewer\n    \
                     agent: reviewer\n    gate: loop-until-clean\n    max_cycles: 0\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("max_cycles"), "{error}");
    }

    #[test]
    fn rejects_a_max_cycles_declared_on_the_first_step() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n    max_cycles: 3\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("producer"), "{error}");
    }

    #[test]
    fn a_step_can_declare_itself_as_the_evidence_capturing_step() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: qa
    agent: qa
    gate: loop-until-clean
    evidence: true
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();
        assert!(workflow.steps[1].captures_evidence);
    }

    #[test]
    fn rejects_two_steps_both_declaring_evidence_capture() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: one
    agent: one
    gate: loop-until-clean
    evidence: true
  - role: two
    agent: two
    gate: loop-until-clean
    evidence: true
"#;
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("already set"), "{error}");
    }

    #[test]
    fn a_step_with_no_type_key_defaults_to_agent_kind() {
        let workflow = Workflow::parse_yaml(DEFAULT_YAML).unwrap();
        for step in &workflow.steps {
            assert_eq!(step.kind, StepKind::Agent);
            assert!(step.agent.is_some());
            assert!(step.run.is_none());
        }
    }

    #[test]
    fn a_hook_step_parses_with_its_run_command_and_no_agent() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: lint
    type: hook
    run: "cargo fmt --check"
    gate: loop-until-clean
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();
        assert_eq!(workflow.steps[1].kind, StepKind::Hook);
        assert_eq!(workflow.steps[1].run.as_deref(), Some("cargo fmt --check"));
        assert_eq!(workflow.steps[1].agent, None);
        assert_eq!(workflow.steps[1].gate, Gate::LoopUntilClean);
    }

    #[test]
    fn rejects_an_unknown_type() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: lint\n    \
                     type: bogus\n    run: true\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("unknown type"), "{error}");
    }

    #[test]
    fn a_type_policy_step_is_rejected_as_not_supported_yet_not_as_an_unknown_type() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: lint\n    \
                     type: policy\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        let message = error.to_string();
        assert!(message.contains("not supported yet"), "{message}");
        assert!(!message.contains("unknown type"), "{message}");
    }

    #[test]
    fn rejects_a_hook_step_missing_run() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: lint\n    \
                     type: hook\n    gate: loop-until-clean\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("\"run\""), "{error}");
    }

    #[test]
    fn rejects_a_hook_step_with_a_blank_run() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: lint\n    \
                     type: hook\n    run: \"   \"\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
    }

    #[test]
    fn rejects_a_hook_step_that_also_declares_an_agent() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: lint\n    \
                     type: hook\n    run: \"true\"\n    agent: coder\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("\"agent\""), "{error}");
    }

    #[test]
    fn rejects_an_agent_step_that_also_declares_run() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: lint\n    \
                     agent: reviewer\n    run: \"true\"\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("\"run\""), "{error}");
    }

    #[test]
    fn rejects_a_hook_step_as_the_producer() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    type: hook\n    run: \"true\"\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("producer"), "{error}");
    }

    #[test]
    fn rejects_a_hook_step_declaring_evidence_capture() {
        let yaml = "name: x\nsteps:\n  - role: coder\n    agent: coder\n  - role: lint\n    \
                     type: hook\n    run: \"true\"\n    evidence: true\n";
        let error = Workflow::parse_yaml(yaml).unwrap_err();
        assert!(matches!(error, CoreError::InvalidWorkflow(_)));
        assert!(error.to_string().contains("evidence"), "{error}");
    }

    #[test]
    fn a_hook_step_can_declare_a_budget_like_any_other_gated_step() {
        let yaml = r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: lint
    type: hook
    run: "cargo fmt --check"
    gate: loop-until-clean
    budget: extra
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();
        assert_eq!(workflow.steps[1].budget, Some(StepBudget::Extra));
    }

    #[test]
    fn step_kind_round_trips_through_its_string_form() {
        for kind in [StepKind::Agent, StepKind::Hook] {
            assert_eq!(StepKind::parse(kind.as_str()).unwrap(), kind);
        }
        assert!(StepKind::parse("ghost").is_err());
    }
}
