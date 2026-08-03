//! Converts a resolved `Workflow` into the wire shape published once, at run start, via
//! `RunEvent::WorkflowResolved` (issue #107). Pure data transform: no I/O, no dependency on the
//! run's persisted state -- everything it needs is already on `Workflow` itself.

use warden_core::{RunEvent, StepTarget, Workflow, WorkflowStepWire};

/// Builds the [`RunEvent::WorkflowResolved`] event for `workflow`, resolving every step's
/// transition target (a [`StepTarget`]) down to the string form the wire uses: another step's own
/// id, or the terminal `"converged"` / `"failed"`.
pub(super) fn resolve_workflow_event(workflow: &Workflow) -> RunEvent {
    let steps = workflow
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| WorkflowStepWire {
            index: index as u32,
            id: step.role.as_str().to_string(),
            kind: step.kind.as_str().to_string(),
            on_clean: target_id(workflow, step.transitions.clean),
            on_blocking: target_id(workflow, step.transitions.blocking),
            on_error: target_id(workflow, step.transitions.error),
            max_cycles: step.max_cycles,
            captures_evidence: step.captures_evidence,
        })
        .collect();

    RunEvent::WorkflowResolved {
        name: workflow.name.clone(),
        entry: workflow.entry(),
        steps,
    }
}

fn target_id(workflow: &Workflow, target: StepTarget) -> String {
    match target {
        StepTarget::Step(index) => workflow.steps[index as usize].role.as_str().to_string(),
        StepTarget::Converged => "converged".to_string(),
        StepTarget::Failed => "failed".to_string(),
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
        let event = resolve_workflow_event(&workflow);

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
        let event = resolve_workflow_event(&workflow);

        let RunEvent::WorkflowResolved { entry, .. } = event else {
            panic!("expected WorkflowResolved");
        };
        assert_eq!(entry, 1);
    }
}
