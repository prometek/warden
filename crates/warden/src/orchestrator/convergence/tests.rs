use super::*;

#[test]
fn generic_workflow_has_no_role_ordering_contract() {
    let workflow = Workflow::parse_yaml(
        r#"
name: generic
entry: audit
steps:
  audit:
    type: command
    run: cargo check
    on_clean: implement
    on_blocking: implement
    on_error: failed
  implement:
    type: agent
    agent: writer
    on_clean: converged
    on_blocking: audit
    on_error: failed
"#,
    )
    .unwrap();
    assert_eq!(
        workflow.steps[workflow.entry() as usize].role.as_str(),
        "audit"
    );
}
