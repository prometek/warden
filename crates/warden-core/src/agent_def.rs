//! On-disk form of a **markdown agent definition** (ADR-0013, issue #22
//! Scope A): the file a `--coder-agent`/`--reviewer-agent`/`--tester-agent`
//! flag points at, which finally gives a role a definition of its own rather
//! than leaving its identity in whatever shell string the user hand-wired
//! into the removed `--*-cmd` flags.
//!
//! A definition is a TOML frontmatter block fenced by [`FRONTMATTER_FENCE`],
//! followed by the markdown body -- the role's **system prompt**:
//!
//! ```text
//! +++
//! runner = "command"
//! program = "claude"
//! args = ["-p"]
//! +++
//!
//! You are Warden's coder. Read the JSON payload on stdin ...
//! ```
//!
//! The schema is **warden-native**, deliberately not Claude Code's
//! `.claude/agents/*.md` format (ADR-0013 / Q1): adopting one agent CLI's
//! format would couple Warden to that CLI and break the agent-agnosticism
//! ADR-0005 exists to protect. The `runner` key names which
//! `warden::agent_runner::AgentRunner` maps this definition onto a concrete
//! subprocess invocation; `command` is the raw program+args escape hatch, so
//! a plain script stays a valid target with no capability lost by the
//! `--*-cmd` removal.
//!
//! Pure/parsing shape only -- reading the file off disk lives in
//! `warden::agent_def`, mirroring the `warden_core::agent_wire` /
//! `warden::process` split. Built here with the same "wire struct private +
//! public type + typed constructor + validated `parse_*` at the boundary"
//! convention as `agent_wire`/`ci_channel`/`evidence_wire`: unknown keys,
//! an unknown runner, and a blank system prompt are all typed errors, never
//! silently defaulted.

use serde::Deserialize;

use crate::error::{CoreError, Result};

/// Fences the TOML frontmatter block, opening and closing (Hugo's `+++`
/// convention). TOML rather than YAML frontmatter: the schema is
/// warden-native anyway (ADR-0013 / Q1), TOML is already this workspace's
/// configuration language, and the maintained YAML crates in the Rust
/// ecosystem are a worse dependency than `toml` for a block this small.
pub const FRONTMATTER_FENCE: &str = "+++";

/// On-disk shape of the frontmatter block, before validation into
/// [`RunnerKind`]. Private, exactly like `agent_wire`'s wire structs: the
/// public type is the validated one.
///
/// `deny_unknown_fields` is the project's stated default (ADR-0013 / Q3): a
/// key Warden does not understand is a definition whose author expected
/// something Warden will not do -- accepting it silently is the fallback
/// code-standards.md forbids, so it is rejected outright with the offending
/// key named.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentDefinitionFrontmatter {
    runner: String,
    program: Option<String>,
    #[serde(default)]
    args: Vec<String>,
}

/// Which runner turns a definition into a concrete subprocess invocation,
/// carrying that runner's own configuration -- so "a `program` without a
/// `command` runner" is unrepresentable rather than merely validated.
///
/// One variant today. The seam that makes this extensible is the
/// `warden::agent_runner::AgentRunner` **trait** (ADR-0013 / Q1), resolved
/// at compile time like `warden::gate_trigger::GateTrigger`; this enum is
/// only the closed set of runner names a definition may legally *name*,
/// validated at the boundary exactly like `EvidenceTool::parse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerKind {
    /// The raw program+args escape hatch (ADR-0013 / Q4): the definition
    /// declares the exact binary and arguments to exec, so a plain script
    /// remains a first-class agent target after `--*-cmd`'s removal.
    /// Arguments are an explicit list -- no shell-style whitespace splitting
    /// (the naive split `parse_agent_command` did, which mangled any
    /// argument containing a space, is gone with those flags).
    Command { program: String, args: Vec<String> },
}

impl RunnerKind {
    /// The runner name accepted in a definition's `runner` key.
    pub const COMMAND: &'static str = "command";
}

/// A parsed, validated markdown agent definition (ADR-0013).
///
/// Never built by hand from raw strings: [`AgentDefinition::new`] and
/// [`parse_agent_definition`] enforce the same invariants (a non-blank
/// system prompt), following `AgentInputMessage`'s convention.
///
/// Deliberately **has no `timeout` key** (issue #22): a per-invocation agent
/// timeout is a separate, undecided concern (it needs a default budget, a
/// config surface, and a call on whether a coder timeout fails the run while
/// a reviewer/tester timeout becomes a blocking finding). A `timeout` key
/// here would either half-implement that or be accepted and ignored -- both
/// worse than not offering it until that ticket lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    pub runner: RunnerKind,
    /// The markdown body after the frontmatter: what this role *is*. Rides
    /// the stdin payload as `AgentInputMessage::system_prompt` (ADR-0013 /
    /// Q2), never argv or a temp file. Whitespace-trimmed, and never blank
    /// -- a definition whose body says nothing configures nothing.
    pub system_prompt: String,
}

impl AgentDefinition {
    /// Validates a definition's invariants at construction with the same
    /// rigor [`parse_agent_definition`] applies on the read side (the
    /// convention `AgentInputMessage::for_coder` established): a blank
    /// (empty or all-whitespace) system prompt is a typed error, never a
    /// silently-accepted empty default.
    ///
    /// The prompt is stored trimmed -- leading/trailing blank lines around a
    /// markdown body are formatting, not content.
    pub fn new(runner: RunnerKind, system_prompt: impl Into<String>) -> Result<Self> {
        let system_prompt = system_prompt.into();
        let trimmed = system_prompt.trim();
        if trimmed.is_empty() {
            return Err(CoreError::MalformedAgentDefinition(
                "agent definition system prompt (the markdown body after the frontmatter) must \
                 not be blank"
                    .to_string(),
            ));
        }
        Ok(Self {
            runner,
            system_prompt: trimmed.to_string(),
        })
    }
}

/// Parses one markdown agent definition, with the same rigor as
/// `parse_agent_input_message`/`parse_ci_result_message`: a missing or
/// unterminated frontmatter fence, malformed TOML, an unknown key, an
/// unknown `runner`, a `command` runner without a usable `program`, or a
/// blank system prompt are all typed errors -- never silently defaulted
/// (code-standards.md: "valider toute entrée externe ... à la frontière").
pub fn parse_agent_definition(raw: &str) -> Result<AgentDefinition> {
    let (frontmatter, body) = split_frontmatter(raw)?;

    let frontmatter: AgentDefinitionFrontmatter = toml::from_str(frontmatter).map_err(|error| {
        CoreError::MalformedAgentDefinition(format!("invalid frontmatter: {error}"))
    })?;

    AgentDefinition::new(parse_runner(frontmatter)?, body)
}

/// Splits `+++\n<frontmatter>\n+++\n<body>` into its two halves.
///
/// Strict about both fences: a file that merely *looks* like a definition
/// (no fence at all, or an opening fence that is never closed) is rejected
/// naming what was expected, rather than being silently read as a
/// prompt-only file with default configuration.
fn split_frontmatter(raw: &str) -> Result<(&str, &str)> {
    let opening_fence = format!("{FRONTMATTER_FENCE}\n");
    let rest = raw.strip_prefix(&opening_fence).ok_or_else(|| {
        CoreError::MalformedAgentDefinition(format!(
            "agent definition must start with a `{FRONTMATTER_FENCE}` frontmatter fence on its \
             own first line"
        ))
    })?;

    let closing_fence = format!("\n{FRONTMATTER_FENCE}\n");
    if let Some((frontmatter, body)) = rest.split_once(&closing_fence) {
        return Ok((frontmatter, body));
    }
    // A file ending exactly at its closing fence: no body at all. Reported
    // as the blank-prompt error it really is (by `AgentDefinition::new`),
    // not as a bogus "missing closing fence".
    if let Some(frontmatter) = rest.strip_suffix(&format!("\n{FRONTMATTER_FENCE}")) {
        return Ok((frontmatter, ""));
    }
    Err(CoreError::MalformedAgentDefinition(format!(
        "agent definition frontmatter is never closed by a `{FRONTMATTER_FENCE}` fence on its \
         own line"
    )))
}

/// Validates the frontmatter's runner selector plus that runner's own keys
/// into a [`RunnerKind`]. An unknown runner name is a typed error against a
/// closed set (`EvidenceTool::parse`'s convention), never a fallback to
/// whatever the single runner of the day happens to be.
fn parse_runner(frontmatter: AgentDefinitionFrontmatter) -> Result<RunnerKind> {
    match frontmatter.runner.as_str() {
        RunnerKind::COMMAND => {
            let program = frontmatter
                .program
                .filter(|program| !program.trim().is_empty())
                .ok_or_else(|| {
                    CoreError::MalformedAgentDefinition(format!(
                        "`runner = \"{}\"` requires a non-blank `program`",
                        RunnerKind::COMMAND
                    ))
                })?;
            Ok(RunnerKind::Command {
                program,
                args: frontmatter.args,
            })
        }
        unknown => Err(CoreError::UnknownRunner(unknown.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND_DEFINITION: &str = r#"+++
runner = "command"
program = "claude"
args = ["-p", "--output-format", "stream-json"]
+++

You are Warden's coder.

Read the JSON payload on stdin.
"#;

    #[test]
    fn parses_a_command_definition_with_its_program_args_and_system_prompt() {
        let definition = parse_agent_definition(COMMAND_DEFINITION).unwrap();
        assert_eq!(
            definition.runner,
            RunnerKind::Command {
                program: "claude".to_string(),
                args: vec![
                    "-p".to_string(),
                    "--output-format".to_string(),
                    "stream-json".to_string()
                ],
            }
        );
        assert_eq!(
            definition.system_prompt,
            "You are Warden's coder.\n\nRead the JSON payload on stdin."
        );
    }

    /// The `args` list is optional -- a definition naming a bare program
    /// (the common "just run my script" case) needs no ceremony.
    #[test]
    fn args_default_to_an_empty_list_when_omitted() {
        let raw = "+++\nrunner = \"command\"\nprogram = \"./reviewer.sh\"\n+++\nreview it\n";
        let definition = parse_agent_definition(raw).unwrap();
        assert_eq!(
            definition.runner,
            RunnerKind::Command {
                program: "./reviewer.sh".to_string(),
                args: Vec::new(),
            }
        );
        assert_eq!(definition.system_prompt, "review it");
    }

    /// An argument containing spaces survives intact -- the capability the
    /// removed `--*-cmd` flags' naive whitespace split never had.
    #[test]
    fn an_argument_containing_whitespace_is_preserved_as_a_single_argument() {
        let raw = "+++\nrunner = \"command\"\nprogram = \"sh\"\nargs = [\"-c\", \"echo one two\"]\n+++\nprompt\n";
        let definition = parse_agent_definition(raw).unwrap();
        assert_eq!(
            definition.runner,
            RunnerKind::Command {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), "echo one two".to_string()],
            }
        );
    }

    /// Q3 (ADR-0013): unknown keys are rejected, naming the offending key --
    /// silently ignoring one would run an agent configured differently than
    /// its author asked for.
    #[test]
    fn rejects_an_unknown_frontmatter_key() {
        let raw = "+++\nrunner = \"command\"\nprogram = \"sh\"\nmodel = \"opus\"\n+++\nprompt\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentDefinition(_)));
        assert!(error.to_string().contains("model"), "{error}");
    }

    /// Issue #22 explicitly keeps a per-invocation agent timeout out of
    /// scope, so `timeout` must not be quietly accepted here (which would
    /// half-implement it) -- it falls under the unknown-key rejection like
    /// any other key Warden does not honour.
    #[test]
    fn rejects_a_timeout_key_rather_than_half_implementing_it() {
        let raw = "+++\nrunner = \"command\"\nprogram = \"sh\"\ntimeout = 30\n+++\nprompt\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentDefinition(_)));
        assert!(error.to_string().contains("timeout"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_runner() {
        let raw = "+++\nrunner = \"telepathy\"\nprogram = \"sh\"\n+++\nprompt\n";
        assert!(matches!(
            parse_agent_definition(raw),
            Err(CoreError::UnknownRunner(runner)) if runner == "telepathy"
        ));
    }

    #[test]
    fn rejects_a_command_runner_without_a_program() {
        let raw = "+++\nrunner = \"command\"\n+++\nprompt\n";
        assert!(matches!(
            parse_agent_definition(raw),
            Err(CoreError::MalformedAgentDefinition(_))
        ));
    }

    #[test]
    fn rejects_a_command_runner_whose_program_is_blank() {
        let raw = "+++\nrunner = \"command\"\nprogram = \"   \"\n+++\nprompt\n";
        assert!(matches!(
            parse_agent_definition(raw),
            Err(CoreError::MalformedAgentDefinition(_))
        ));
    }

    /// Q3 (ADR-0013): a definition with nothing to say about what the role
    /// *is* configures nothing -- a typed error, never a silent default.
    #[test]
    fn rejects_a_blank_system_prompt() {
        let empty_body = "+++\nrunner = \"command\"\nprogram = \"sh\"\n+++\n";
        assert!(matches!(
            parse_agent_definition(empty_body),
            Err(CoreError::MalformedAgentDefinition(_))
        ));

        let whitespace_body = "+++\nrunner = \"command\"\nprogram = \"sh\"\n+++\n  \n\t\n";
        assert!(matches!(
            parse_agent_definition(whitespace_body),
            Err(CoreError::MalformedAgentDefinition(_))
        ));

        let no_body_at_all = "+++\nrunner = \"command\"\nprogram = \"sh\"\n+++";
        assert!(matches!(
            parse_agent_definition(no_body_at_all),
            Err(CoreError::MalformedAgentDefinition(_))
        ));
    }

    #[test]
    fn rejects_a_file_with_no_frontmatter_fence_at_all() {
        assert!(matches!(
            parse_agent_definition("You are Warden's coder.\n"),
            Err(CoreError::MalformedAgentDefinition(_))
        ));
    }

    #[test]
    fn rejects_frontmatter_that_is_never_closed() {
        let raw = "+++\nrunner = \"command\"\nprogram = \"sh\"\nprompt\n";
        assert!(matches!(
            parse_agent_definition(raw),
            Err(CoreError::MalformedAgentDefinition(_))
        ));
    }

    #[test]
    fn rejects_malformed_toml_frontmatter() {
        let raw = "+++\nrunner = = \"command\"\n+++\nprompt\n";
        assert!(matches!(
            parse_agent_definition(raw),
            Err(CoreError::MalformedAgentDefinition(_))
        ));
    }

    /// `new` enforces the same invariant the parser does, so a caller
    /// constructing a definition programmatically (a test fixture, a future
    /// config source) can never produce one `parse_agent_definition` would
    /// refuse.
    #[test]
    fn new_rejects_a_blank_system_prompt_like_the_parser_does() {
        let runner = RunnerKind::Command {
            program: "sh".to_string(),
            args: Vec::new(),
        };
        assert!(matches!(
            AgentDefinition::new(runner.clone(), "  \n\t"),
            Err(CoreError::MalformedAgentDefinition(_))
        ));
        assert_eq!(
            AgentDefinition::new(runner, "  be a coder\n")
                .unwrap()
                .system_prompt,
            "be a coder"
        );
    }

    /// A `+++` inside the prompt body must not be mistaken for the fence --
    /// only the first closing fence terminates the frontmatter.
    #[test]
    fn a_fence_like_line_inside_the_body_stays_part_of_the_prompt() {
        let raw = "+++\nrunner = \"command\"\nprogram = \"sh\"\n+++\nprompt\n+++\nmore prompt\n";
        let definition = parse_agent_definition(raw).unwrap();
        assert_eq!(definition.system_prompt, "prompt\n+++\nmore prompt");
    }
}
