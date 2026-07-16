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
///
/// Private: the fence is an implementation detail of
/// [`parse_agent_definition`], which is the only way in. Callers parse
/// definitions, they don't assemble them.
const FRONTMATTER_FENCE: &str = "+++";

/// Step 1 of parsing the frontmatter: read *only* the runner selector, so
/// the runner-specific shape to validate against is known before any of its
/// keys are looked at. Deliberately **not** `deny_unknown_fields` -- judging
/// keys is step 2's job, and it cannot happen until the runner is known.
#[derive(Debug, Deserialize)]
struct RunnerSelector {
    runner: String,
}

/// Step 2 for `runner = "command"`: the keys that runner -- and only that
/// runner -- understands. Private, exactly like `agent_wire`'s wire structs:
/// the public type is the validated one.
///
/// **Each runner gets its own struct**, rather than one flat struct holding
/// every runner's keys (ADR-0013 amendment, issue #22 review): with a flat
/// shape, `deny_unknown_fields` only rejects keys *no* runner knows, so the
/// moment a second runner lands, `runner = "command"` plus a key belonging
/// to that other runner would be accepted by serde (the field is known) and
/// then silently dropped by the `Command` arm -- precisely the
/// accepted-then-ignored failure `deny_unknown_fields` was chosen to
/// prevent, and precisely what the runner seam exists to make likely. Keys
/// scoped per runner make that a typed error instead, naming the key.
///
/// `runner` is repeated here because `deny_unknown_fields` sees the whole
/// frontmatter table, tag included.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandRunnerFrontmatter {
    #[allow(
        dead_code,
        reason = "consumed by RunnerSelector; declared so \
        deny_unknown_fields accepts the tag it dispatched on"
    )]
    runner: String,
    program: String,
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
    ///
    /// **A relative `program` (or a path in `args`) resolves against the
    /// agent's worktree** -- `warden::process::spawn` sets the child's `cwd`
    /// to it before exec (code-standards.md § Agent Subprocess Protocol), so
    /// `program = "./reviewer.sh"` runs *the repository's own copy of that
    /// script, as committed at the commit under review*. For a
    /// reviewer/tester that is a real hazard: the coder can rewrite and
    /// commit that file, and the reviewer would then execute coder-authored
    /// code, defeating the independence the pipeline rests on. Prefer an
    /// absolute path for those two roles. Not enforced here: the behaviour
    /// predates markdown definitions (`--reviewer-cmd "sh ./reviewer.sh"`
    /// resolved identically) and refusing relative paths is a product call
    /// that would break the plain-script case this escape hatch exists for
    /// -- see ADR-0013's Conséquences.
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
    AgentDefinition::new(parse_runner(frontmatter)?, body)
}

/// A UTF-8 BOM: legal in a text file, and invisible in an editor, but it
/// sits *before* the opening fence and would make the fence check fail while
/// the first line visibly reads `+++`.
const BYTE_ORDER_MARK: &str = "\u{feff}";

/// Splits `+++\n<frontmatter>\n+++\n<body>` into its two halves.
///
/// Strict about both fences: a file that merely *looks* like a definition
/// (no fence at all, or an opening fence that is never closed) is rejected
/// naming what was expected, rather than being silently read as a
/// prompt-only file with default configuration.
fn split_frontmatter(raw: &str) -> Result<(&str, &str)> {
    // A CRLF or BOM file would otherwise fail the fence check below with
    // "must start with a `+++` fence on its own first line" -- about a file
    // whose first line *is* visibly `+++` (issue #22 review, LOW). Loud but
    // actively misleading is its own bug, and a `.gitattributes`
    // `text eol=crlf` checkout makes it reachable. Named explicitly instead;
    // deliberately *not* normalised away, since silently rewriting the input
    // would also rewrite the system prompt (stray `\r` in every line of the
    // body) rather than just the fences.
    if let Some(rest) = raw.strip_prefix(BYTE_ORDER_MARK) {
        let hint = if rest.starts_with(FRONTMATTER_FENCE) {
            " (the `+++` fence itself looks fine -- the BOM is what precedes it)"
        } else {
            ""
        };
        return Err(CoreError::MalformedAgentDefinition(format!(
            "agent definition starts with a UTF-8 byte order mark{hint}; save it without a BOM"
        )));
    }
    if raw.starts_with(&format!("{FRONTMATTER_FENCE}\r\n")) {
        return Err(CoreError::MalformedAgentDefinition(format!(
            "agent definition uses CRLF line endings; warden agent definitions must use LF \
             (the `{FRONTMATTER_FENCE}` fence itself looks fine -- the line ending is what \
             doesn't)"
        )));
    }

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

/// Validates the frontmatter's runner selector, then that runner's own keys,
/// into a [`RunnerKind`].
///
/// Two passes over the same table, deliberately: which keys are legal is a
/// function of *which runner* the definition names, so the selector has to be
/// read before anything else can be judged (see
/// [`CommandRunnerFrontmatter`]). An unknown runner name is a typed error
/// against a closed set (`EvidenceTool::parse`'s convention), never a
/// fallback to whatever the single runner of the day happens to be -- and it
/// is reported *before* any key complaint, since a key can't be wrong until
/// it's known what it was supposed to configure.
fn parse_runner(frontmatter: &str) -> Result<RunnerKind> {
    let selector: RunnerSelector = toml::from_str(frontmatter).map_err(invalid_frontmatter)?;

    match selector.runner.as_str() {
        RunnerKind::COMMAND => {
            let command: CommandRunnerFrontmatter =
                toml::from_str(frontmatter).map_err(invalid_frontmatter)?;
            if command.program.trim().is_empty() {
                return Err(CoreError::MalformedAgentDefinition(format!(
                    "`runner = \"{}\"` requires a non-blank `program`",
                    RunnerKind::COMMAND
                )));
            }
            Ok(RunnerKind::Command {
                program: command.program,
                args: command.args,
            })
        }
        unknown => Err(CoreError::UnknownRunner(unknown.to_string())),
    }
}

fn invalid_frontmatter(error: toml::de::Error) -> CoreError {
    CoreError::MalformedAgentDefinition(format!("invalid frontmatter: {error}"))
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
    /// needs no ceremony.
    ///
    /// The fixture is deliberately an *absolute* path: a relative `program`
    /// parses just as well (nothing here resolves it), but it would resolve
    /// against the worktree under review at spawn time, which is the wrong
    /// thing to model as "typical" for a reviewer -- see [`RunnerKind`].
    #[test]
    fn args_default_to_an_empty_list_when_omitted() {
        let raw =
            "+++\nrunner = \"command\"\nprogram = \"/opt/warden/reviewer.sh\"\n+++\nreview it\n";
        let definition = parse_agent_definition(raw).unwrap();
        assert_eq!(
            definition.runner,
            RunnerKind::Command {
                program: "/opt/warden/reviewer.sh".to_string(),
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

    /// Runner keys are scoped to their runner (ADR-0013 amendment, issue
    /// #22 review): `program`/`args` are the `command` runner's own keys, so
    /// they are validated against *that* runner's shape rather than a flat
    /// table every runner shares. With a flat table, a second runner's key
    /// would be accepted-then-ignored under `runner = "command"` -- exactly
    /// what `deny_unknown_fields` is here to prevent. Pinned via the
    /// observable consequence: anything outside the command runner's own key
    /// set is refused, naming the key.
    #[test]
    fn a_key_outside_the_command_runners_own_set_is_rejected_naming_it() {
        let raw =
            "+++\nrunner = \"command\"\nprogram = \"sh\"\nendpoint = \"https://x\"\n+++\nprompt\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentDefinition(_)));
        assert!(error.to_string().contains("endpoint"), "{error}");
    }

    /// Precedence of the two-pass parse: the runner is judged first. A key
    /// can't be reported as wrong before it's known what runner it was meant
    /// to configure, so an unknown runner wins over any key complaint.
    #[test]
    fn an_unknown_runner_is_reported_before_its_keys_are_judged() {
        let raw = "+++\nrunner = \"telepathy\"\nbrainwave = \"alpha\"\n+++\nprompt\n";
        assert!(matches!(
            parse_agent_definition(raw),
            Err(CoreError::UnknownRunner(runner)) if runner == "telepathy"
        ));
    }

    /// LOW (issue #22 review): a CRLF file's first line visibly *is* `+++`,
    /// so "must start with a `+++` fence" would be loud but actively
    /// misdirecting. The error must name the real cause -- and the file must
    /// still be rejected, not silently normalised (that would rewrite the
    /// system prompt too, not just the fences).
    #[test]
    fn a_crlf_definition_is_rejected_naming_the_line_endings_not_the_fence() {
        let raw = "+++\r\nrunner = \"command\"\r\nprogram = \"sh\"\r\n+++\r\nprompt\r\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentDefinition(_)));
        let rendered = error.to_string();
        assert!(rendered.contains("CRLF"), "{rendered}");
    }

    /// Same misdirection, invisible cause: a BOM sits before the fence.
    #[test]
    fn a_bom_prefixed_definition_is_rejected_naming_the_bom_not_the_fence() {
        let raw = "\u{feff}+++\nrunner = \"command\"\nprogram = \"sh\"\n+++\nprompt\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentDefinition(_)));
        let rendered = error.to_string();
        assert!(rendered.contains("byte order mark"), "{rendered}");
    }

    #[test]
    fn rejects_an_unknown_runner() {
        let raw = "+++\nrunner = \"telepathy\"\nprogram = \"sh\"\n+++\nprompt\n";
        assert!(matches!(
            parse_agent_definition(raw),
            Err(CoreError::UnknownRunner(runner)) if runner == "telepathy"
        ));
    }

    /// The two-pass parse reads the `runner` selector first (`RunnerSelector`);
    /// a frontmatter with no `runner` key at all has nothing to dispatch on,
    /// so it must be a typed error at the boundary rather than a panic or a
    /// default-runner fallback. Probes the first pass in isolation -- every
    /// other rejection test names a runner, so none of them exercised a
    /// wholly absent selector.
    #[test]
    fn rejects_frontmatter_with_no_runner_key_at_all() {
        let raw = "+++\nprogram = \"sh\"\nargs = [\"-c\", \"true\"]\n+++\nprompt\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(
            matches!(error, CoreError::MalformedAgentDefinition(_)),
            "a frontmatter with no runner selector must be a typed error, got {error:?}"
        );
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
