//! Issue #73: the user-definable pipeline. Before this, the pipeline was
//! wired directly into code -- a closed `AgentRole { Coder, Reviewer, Tester }`
//! (`crate::state::AgentRole`) and a hardcoded coder -> gate review -> gate
//! test sequence (`warden::orchestrator`). [`Workflow`] moves that sequence
//! from code to **data** a user can define in `.warden/workflow.yaml`:
//!
//! ```yaml
//! name: default
//! steps:
//!   - role: coder
//!     agent: coder
//!   - role: reviewer
//!     agent: code-reviewer
//!     gate: loop-until-clean
//!   - role: tester
//!     agent: test-runner
//!     gate: loop-until-clean
//! ```
//!
//! [`Workflow::builtin_default`] is exactly this shape -- what a run uses
//! when no `.warden/workflow.yaml` exists at all, so the **absence** of a
//! workflow file reproduces today's pipeline exactly (strict retro-compat,
//! the acceptance criterion issue #73 calls out as the most important one).
//!
//! Deliberately linear (issue #73 "out of scope"): a [`Workflow`] is a plain
//! ordered list of [`WorkflowStep`]s, each an open, named [`Role`] resolved
//! to an agent, with an optional [`Gate`]. No DAG, no conditional branches,
//! no parallel steps -- every step but the first (the "producer", coder-like
//! role that has no gate of its own) may loop back to the first step when its
//! own gate finds a blocking problem, exactly like the reviewer/tester
//! already did. [`Gate`] is deliberately a small, explicit enum rather than a
//! free-form string precisely so it stays *extensible* without a schema
//! change: adding a new gate kind later is a new variant plus two match arms
//! here, never a change to this module's shape.
//!
//! Pure/parsing shape only, mirroring every other user-facing file this
//! crate parses (`agent_def`, `ci_channel`, `evidence_wire`): reading
//! `.warden/workflow.yaml` off disk lives in `warden::agent_def` (I/O), this
//! module only knows the schema and its invariants.

use serde::Deserialize;

use crate::error::{CoreError, Result};

/// A step's name in the pipeline (`steps[].role` in `workflow.yaml`) --
/// open, unlike the closed `AgentRole` the built-in coder/reviewer/tester
/// path still uses internally (see this module's own docs and
/// `crate::state::AgentRole`'s). Any non-blank string is a legal role name:
/// `"coder"`/`"reviewer"`/`"tester"` are not special-cased by this type at
/// all -- they're just the names the *built-in* default workflow happens to
/// use. A custom workflow can name a step anything (`"techlead"`, `"docs"`,
/// ...); [`crate::FindingSource::role`] and [`crate::decide_next_state_for_step`]
/// key off exactly this string, so a custom role's findings aggregate in the
/// convergence loop the same way a reviewer's or tester's already do.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Role(String);

impl Role {
    /// Rejects a blank (empty or all-whitespace) name -- a step with no real
    /// name can never be meaningfully compared against a
    /// [`crate::FindingSource`], so this is validated once, at construction,
    /// rather than left for every later comparison to silently no-op
    /// against.
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

/// How a step's findings gate the pipeline. Deliberately a closed enum, not
/// a free-form string: an unknown gate name in `workflow.yaml` must be a
/// clear parse error naming the bad value, never silently treated as one of
/// the two known kinds. Extensible in principle (issue #73: "conçu pour
/// être extensible") -- a future kind is a new variant plus two match arms
/// here (`as_str`/`parse`), not a change to [`WorkflowStep`]'s shape or to
/// any caller's signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// No gate at all: the step runs once per cycle and never reboucles the
    /// pipeline back to the first step, whatever it reports. This is the
    /// first step's own implicit gate (see [`Workflow::validate`]) -- a
    /// producer step (the coder-equivalent) has nothing to gate *on* yet,
    /// since it hasn't produced this cycle's work until it runs.
    PassThrough,
    /// The step's findings gate the pipeline exactly like the reviewer/
    /// tester already do: a blocking finding attributed to this step's own
    /// role reboucles the whole pipeline back to the first step (within this
    /// step's own cycle budget), instead of letting the pipeline advance.
    LoopUntilClean,
}

impl Gate {
    pub fn as_str(self) -> &'static str {
        match self {
            Gate::PassThrough => "pass-through",
            Gate::LoopUntilClean => "loop-until-clean",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "pass-through" => Ok(Gate::PassThrough),
            "loop-until-clean" => Ok(Gate::LoopUntilClean),
            other => Err(CoreError::InvalidWorkflow(format!(
                "unknown gate {other:?} (expected \"loop-until-clean\", or omit the key for a \
                 plain pass-through)"
            ))),
        }
    }
}

/// One step of a [`Workflow`]: a [`Role`] resolved to an `agent` (a name
/// `warden::agent_def` resolves onto a markdown agent definition, ADR-0013 --
/// e.g. `.claude/agents/<agent>.md` for a role beyond the built-in three),
/// gated by an optional [`Gate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowStep {
    pub role: Role,
    /// The agent definition name this step resolves to -- not necessarily
    /// the same string as `role` (`workflow.yaml`'s own example: `role:
    /// reviewer` / `agent: code-reviewer`), since a workflow may want two
    /// steps sharing a role's *function* to still be told apart by name, or
    /// simply prefer a differently-named agent file for a role.
    pub agent: String,
    pub gate: Gate,
}

/// A user-definable pipeline (issue #73): an ordered, non-empty list of
/// [`WorkflowStep`]s. `steps[0]` is always the **producer** -- the
/// coder-equivalent role that does this cycle's actual work -- and every
/// later step gates on that cycle's work, looping back to `steps[0]` on a
/// blocking finding attributed to its own role (see
/// [`crate::decide_next_state_for_step`]). Deliberately linear: no DAG, no
/// conditional branching, no parallel steps (issue #73 "out of scope").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    pub name: String,
    pub steps: Vec<WorkflowStep>,
}

/// Wire shape of one `workflow.yaml` step -- `gate` absent means "plain
/// pass-through" (never "reject", never "assume loop-until-clean": an
/// omitted key and a wrong one must not be conflated, see [`Gate::parse`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowStepWire {
    role: String,
    agent: String,
    gate: Option<String>,
}

/// Wire shape of `.warden/workflow.yaml` itself.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowWire {
    name: String,
    steps: Vec<WorkflowStepWire>,
}

impl Workflow {
    /// The pipeline every run drives when no `.warden/workflow.yaml` exists
    /// at all -- the exact shape `workflow.yaml`'s own doc-comment example
    /// carries, and the one this crate's tests pin against the pre-issue-#73
    /// pipeline (coder -> gate review -> gate test): this is issue #73's
    /// central retro-compat guarantee, made an explicit, testable value
    /// rather than an implicit "well, the code just happens to still do
    /// that" claim.
    pub fn builtin_default() -> Self {
        // Constructed from already-valid literals -- `expect` here is an
        // invariant on this crate's own hardcoded default, never on
        // user-controlled input (`parse_yaml`'s job).
        Self {
            name: "default".to_string(),
            steps: vec![
                WorkflowStep {
                    role: Role::new("coder").expect("literal role name is never blank"),
                    agent: "coder".to_string(),
                    gate: Gate::PassThrough,
                },
                WorkflowStep {
                    role: Role::new("reviewer").expect("literal role name is never blank"),
                    agent: "code-reviewer".to_string(),
                    gate: Gate::LoopUntilClean,
                },
                WorkflowStep {
                    role: Role::new("tester").expect("literal role name is never blank"),
                    agent: "test-runner".to_string(),
                    gate: Gate::LoopUntilClean,
                },
            ],
        }
    }

    /// Parses and validates a `.warden/workflow.yaml` document. Every
    /// failure is a [`CoreError::InvalidWorkflow`] naming *what* is wrong
    /// (malformed YAML, an unknown key, a blank role/agent, a duplicate
    /// role, an unknown gate, a first step that isn't a plain
    /// pass-through) -- the caller (`warden::agent_def`, which reads the
    /// file) names *which file*, never silently falling back to
    /// [`Self::builtin_default`] on a parse failure (code-standards.md: no
    /// silent fallback -- a malformed workflow file must fail the run, not
    /// quietly run the default pipeline instead).
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
        for (index, step) in wire.steps.into_iter().enumerate() {
            let role = Role::new(step.role)
                .map_err(|error| CoreError::InvalidWorkflow(format!("step {index}: {error}")))?;
            if step.agent.trim().is_empty() {
                return Err(CoreError::InvalidWorkflow(format!(
                    "step {index} (role {role:?}): agent must not be blank"
                )));
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
                         \"loop-until-clean\", or omit the key for a plain pass-through)"
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
            steps.push(WorkflowStep {
                role,
                agent: step.agent,
                gate,
            });
        }

        Ok(Self {
            name: wire.name,
            steps,
        })
    }

    /// The step this pipeline reboucles to when a later step's gate finds a
    /// blocking problem -- always the first one (issue #73: linear sequence,
    /// no DAG). Named rather than every caller writing `steps[0]` directly,
    /// so the "reboucle target is always the producer" invariant has exactly
    /// one place it's stated.
    pub fn producer_role(&self) -> &Role {
        &self.steps[0].role
    }

    /// `true` when `step_index` is this workflow's last step -- the point at
    /// which a clean gate converges the run instead of advancing to the next
    /// step (see [`crate::decide_next_state_for_step`]).
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
  - role: tester
    agent: test-runner
    gate: loop-until-clean
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
        assert_eq!(workflow.steps[1].role.as_str(), "reviewer");
        assert_eq!(workflow.steps[1].gate, Gate::LoopUntilClean);
        assert_eq!(workflow.steps[2].role.as_str(), "tester");
        assert_eq!(workflow.steps[2].gate, Gate::LoopUntilClean);
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
        for gate in [Gate::PassThrough, Gate::LoopUntilClean] {
            assert_eq!(Gate::parse(gate.as_str()).unwrap(), gate);
        }
        assert!(Gate::parse("ghost").is_err());
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
}
