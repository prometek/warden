use super::*;

#[test]
fn agent_command_preserves_program_and_arguments() {
    let command = AgentCommand::new("agent", ["--json", "value"]);
    assert_eq!(command.program, "agent");
    assert_eq!(command.args, ["--json", "value"]);
}
