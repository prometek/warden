//! The **runner seam** (ADR-0013 / Q1, issue #22 Scope A): maps a parsed
//! markdown agent definition (`warden_core::AgentDefinition`) onto the
//! concrete subprocess invocation [`crate::process::spawn`] executes.
//!
//! Shaped exactly like [`crate::gate_trigger::GateTrigger`], the existing
//! injection-seam precedent in this crate: a trait resolved at **compile
//! time**, a generic call site, one concrete implementation used in
//! production, and a fake substitutable in tests. Not a config-declared
//! registry -- a definition's `runner` key names *which* runner it is
//! written for (validated against a closed set at the boundary, in
//! `warden_core::agent_def`), it does not itself load anything.
//!
//! A runner receives the **parsed definition** rather than a rendered
//! prompt: the system prompt does not belong in the invocation at all, it
//! rides the stdin payload (`AgentInputMessage::system_prompt`, ADR-0013 /
//! Q2), so there is nothing to render.
//!
//! This is what keeps Warden agent-agnostic (ADR-0005): the definition
//! schema is warden-native, and mapping it onto whatever CLI a user actually
//! runs (`claude`, `aider`, a shell script) is this seam's job. Warden ships
//! no agent of its own.

use warden_core::{AgentDefinition, RunnerKind};

use crate::error::Result;
use crate::process::AgentCommand;

/// Turns a role's definition into the command to spawn for it.
///
/// Fallible on purpose: a runner that cannot honour a definition must say so
/// with a typed error rather than substitute a default invocation
/// (code-standards.md: "no silent fallback").
pub trait AgentRunner {
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand>;
}

/// The production [`AgentRunner`]: executes the program+args a definition
/// declares, verbatim.
///
/// This is the escape hatch that makes `--*-cmd`'s removal lossless
/// (ADR-0013 / Q4) -- a plain script remains a first-class agent target,
/// now declared in a definition that also carries its system prompt, instead
/// of a shell string Warden whitespace-split and knew nothing else about.
pub struct CommandRunner;

impl AgentRunner for CommandRunner {
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand> {
        match &definition.runner {
            // Exhaustive over today's single runner kind: adding a variant
            // must be a compile error here, not a runtime fallback.
            RunnerKind::Command { program, args } => Ok(AgentCommand::new(program, args.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(program: &str, args: &[&str]) -> AgentDefinition {
        AgentDefinition::new(
            RunnerKind::Command {
                program: program.to_string(),
                args: args.iter().map(|arg| arg.to_string()).collect(),
            },
            "be an agent",
        )
        .unwrap()
    }

    #[test]
    fn command_runner_builds_the_program_and_args_the_definition_declares() {
        let command = CommandRunner
            .build_command(&definition("claude", &["-p", "--output-format", "json"]))
            .unwrap();

        assert_eq!(command.program, "claude");
        assert_eq!(command.args, vec!["-p", "--output-format", "json"]);
    }

    #[test]
    fn command_runner_builds_a_bare_program_with_no_args() {
        let command = CommandRunner
            .build_command(&definition("./coder.sh", &[]))
            .unwrap();

        assert_eq!(command.program, "./coder.sh");
        assert!(command.args.is_empty());
    }

    /// The capability the removed `--*-cmd` flags never had: an argument
    /// containing whitespace reaches the child as *one* argument, because a
    /// definition declares an explicit list instead of a shell string
    /// `parse_agent_command` used to split naively.
    #[test]
    fn an_argument_containing_whitespace_reaches_the_command_intact() {
        let command = CommandRunner
            .build_command(&definition("sh", &["-c", "echo one two three"]))
            .unwrap();

        assert_eq!(command.args, vec!["-c", "echo one two three"]);
    }

    /// The system prompt is *not* part of the invocation (ADR-0013 / Q2):
    /// it must never leak into argv, where it would show up in `ps`.
    #[test]
    fn the_system_prompt_never_leaks_into_the_command() {
        let definition = AgentDefinition::new(
            RunnerKind::Command {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), "true".to_string()],
            },
            "SECRET-PROMPT-MARKER",
        )
        .unwrap();

        let command = CommandRunner.build_command(&definition).unwrap();

        assert!(!command.program.contains("SECRET-PROMPT-MARKER"));
        assert!(!command
            .args
            .iter()
            .any(|arg| arg.contains("SECRET-PROMPT-MARKER")));
    }
}
