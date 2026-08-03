//! Converts a resolved `Workflow` into the wire shape published once, at run start, via
//! `RunEvent::WorkflowResolved` (issue #107). Pure data transform: no I/O, no dependency on the
//! run's persisted state -- everything it needs is already on `Workflow` itself.

use warden_core::{RunEvent, StepTarget, Workflow, WorkflowStepWire};

use crate::error::{Result, WardenError};

/// Builds the [`RunEvent::WorkflowResolved`] event for `workflow`, resolving every step's
/// transition target (a [`StepTarget`]) down to the string form the wire uses: another step's own
/// id, or the terminal `"converged"` / `"failed"`.
pub(super) fn resolve_workflow_event(workflow: &Workflow) -> Result<RunEvent> {
    let steps = workflow
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            Ok(WorkflowStepWire {
                index: index as u32,
                id: step.role.as_str().to_string(),
                kind: step.kind.as_str().to_string(),
                on_clean: target_id(workflow, step.transitions.clean)?,
                on_blocking: target_id(workflow, step.transitions.blocking)?,
                on_error: target_id(workflow, step.transitions.error)?,
                max_cycles: step.max_cycles,
                captures_evidence: step.captures_evidence,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(RunEvent::WorkflowResolved {
        name: workflow.name.clone(),
        entry: workflow.entry(),
        steps,
    })
}

/// Resolves one [`StepTarget`] to its wire string -- see [`WardenError::InvalidWorkflowStepTarget`]
/// for the (should-be-unreachable) out-of-bounds case this guards against instead of indexing
/// straight into `workflow.steps`.
fn target_id(workflow: &Workflow, target: StepTarget) -> Result<String> {
    match target {
        StepTarget::Step(index) => workflow
            .steps
            .get(index as usize)
            .map(|step| step.role.as_str().to_string())
            .ok_or(WardenError::InvalidWorkflowStepTarget {
                index,
                declared_steps: workflow.steps.len(),
            }),
        StepTarget::Converged => Ok("converged".to_string()),
        StepTarget::Failed => Ok("failed".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKFLOW: &str = r#"
name: quality-loop
entry: implementation
steps:
  implementation:
    type: agent
    agent: implementer
    on_clean: review
    on_blocking: implementation
    on_error: failed
  review:
    type: agent
    agent: reviewer
    on_clean: verification
    on_blocking: implementation
    on_error: failed
    max_cycles: 3
    evidence: true
  verification:
    type: command
    run: cargo test
    on_clean: converged
    on_blocking: implementation
    on_error: failed
"#;

    #[test]
    fn resolves_every_step_with_its_own_transitions_kind_and_budget() {
        let workflow = Workflow::parse_yaml(WORKFLOW).unwrap();
        let event = resolve_workflow_event(&workflow).unwrap();

        let RunEvent::WorkflowResolved { name, entry, steps } = event else {
            panic!("expected WorkflowResolved");
        };
        assert_eq!(name, "quality-loop");
        assert_eq!(entry, 0);
        assert_eq!(steps.len(), 3);

        let implementation = &steps[0];
        assert_eq!(implementation.id, "implementation");
        assert_eq!(implementation.kind, "agent");
        assert_eq!(implementation.on_clean, "review");
        assert_eq!(implementation.on_blocking, "implementation");
        assert_eq!(implementation.on_error, "failed");
        assert_eq!(implementation.max_cycles, None);
        assert!(!implementation.captures_evidence);

        let review = &steps[1];
        assert_eq!(review.id, "review");
        assert_eq!(review.on_clean, "verification");
        assert_eq!(review.max_cycles, Some(3));
        assert!(review.captures_evidence);

        let verification = &steps[2];
        assert_eq!(verification.id, "verification");
        assert_eq!(verification.kind, "command");
        assert_eq!(verification.on_clean, "converged");
        assert_eq!(verification.on_error, "failed");
    }

    #[test]
    fn resolves_a_non_zero_entry_step() {
        let workflow =
            Workflow::parse_yaml(&WORKFLOW.replace("entry: implementation", "entry: review"))
                .unwrap();
        let event = resolve_workflow_event(&workflow).unwrap();

        let RunEvent::WorkflowResolved { entry, .. } = event else {
            panic!("expected WorkflowResolved");
        };
        assert_eq!(entry, 1);
    }

    #[test]
    fn a_step_target_index_past_the_end_of_steps_is_a_typed_error_not_a_panic() {
        // `Workflow::parse_yaml`'s own `parse_target` never produces such an index -- this only
        // exercises a hand-built `Workflow` (every field is `pub`), the case
        // `WardenError::InvalidWorkflowStepTarget` exists to guard against.
        use warden_core::{Role, StepKind, StepTransitions};

        let workflow = Workflow {
            name: "broken".to_string(),
            entry_step: 0,
            steps: vec![warden_core::WorkflowStep {
                role: Role::new("only-step").unwrap(),
                kind: StepKind::Command,
                agent: None,
                run: Some("true".to_string()),
                transitions: StepTransitions {
                    clean: StepTarget::Step(5),
                    blocking: StepTarget::Converged,
                    error: StepTarget::Failed,
                },
                max_cycles: None,
                captures_evidence: false,
            }],
        };

        let error = resolve_workflow_event(&workflow).unwrap_err();
        assert!(matches!(
            error,
            WardenError::InvalidWorkflowStepTarget {
                index: 5,
                declared_steps: 1,
            }
        ));
    }
}
