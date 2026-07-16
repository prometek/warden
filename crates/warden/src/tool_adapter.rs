//! The **tool adapter seam** (issue #24): maps a parsed markdown agent
//! definition (`warden_core::AgentDefinition`) onto the concrete subprocess
//! invocation [`crate::process::spawn`] executes, one built-in CLI at a
//! time, selected once per run by `--tool <name>`.
//!
//! Shaped exactly like the trait it replaces, ADR-0013's
//! `warden::agent_runner::AgentRunner` -- itself modelled on
//! [`crate::gate_trigger::GateTrigger`] -- a trait resolved at **compile
//! time**, a generic call site, one concrete implementation per tool used in
//! production, a fake substitutable in tests.
//!
//! # Why this replaces the warden-native runner (issue #24, reversing
//! ADR-0013 / Q1)
//!
//! ADR-0013 kept Warden agent-agnostic by making the *definition* schema
//! warden-native (`runner = "command"` + raw `program`/`args`) and pushing
//! all CLI-specific knowledge into a user-authored file. In practice that
//! made a real run too expensive to set up: three markdown files, plus a
//! wrapper script the user had to write by hand to (a) restore `HOME` so an
//! agent CLI could find its own auth (`env_clear()` only ever forwards
//! `PATH`, Architecture.md §10) and (b) translate that CLI's own output
//! format into the findings NDJSON `warden_core::parse_findings` expects.
//! The warden-native frontmatter added essentially nothing over the prompt
//! itself once that wrapper existed.
//!
//! Issue #24's decision: **Warden ships opinionated, built-in adapters** for
//! specific CLIs (`claude` first; `aider` and others are meant to gain their
//! own [`ToolAdapter`] impl later, never a config-declared registry --
//! `--tool <name>` selects one of a closed, compiled-in set, exactly like
//! [`warden_core::RunState`]/`AgentRole` string parsing). Warden is no
//! longer agent-agnostic at the *schema* level (a definition is Claude
//! Code's own `.claude/agents/*.md` shape, `warden_core::agent_def`) --
//! agent-agnosticism now lives here instead: at the trait boundary, a new
//! CLI is a new [`ToolAdapter`] impl, and Warden ships no LLM implementation
//! of its own (ADR-0005 still holds at that level).
//!
//! Each adapter owns everything that used to be the user's problem:
//! - [`ToolAdapter::build_command`] builds the real invocation (program,
//!   args, this tool's own system-prompt flag, its own model flag) from a
//!   definition's `tools`/`model`.
//! - [`ToolAdapter::env_allowlist`] declares the env vars (beyond `PATH`)
//!   this tool needs to run at all -- see [`crate::process::spawn`]'s docs
//!   for the Architecture.md §10 relaxation this requires.
//! - [`ToolAdapter::extract_findings`] turns a reviewer/tester invocation's
//!   raw captured stdout into the findings it reported, so a user never
//!   writes an output-translation wrapper again.
//! - [`ToolAdapter::default_prompt`] gives every role something to run even
//!   when the run's base repo has no `.warden/agents/<role>.md` at all
//!   (`warden::agent_def::resolve_agent_definition`) -- the
//!   `warden run --repo ... --intent ... --tool claude` zero-`.md` UX issue
//!   #24 exists to enable.

use warden_core::{AgentDefinition, AgentRole, Finding};

use crate::error::Result;
use crate::process::AgentCommand;

/// Turns a role's definition into the command to spawn for it, plus
/// everything else specific to one tool CLI.
///
/// [`build_command`](ToolAdapter::build_command) is fallible on purpose: an
/// adapter that cannot honour a definition must say so with a typed error
/// rather than substitute a default invocation (code-standards.md: "no
/// silent fallback"). The other three methods are infallible where the
/// underlying operation genuinely cannot fail structurally (env allowlists
/// and default prompts are compiled-in constants); only
/// [`extract_findings`](ToolAdapter::extract_findings) can fail, since it
/// parses untrusted subprocess output.
pub trait ToolAdapter {
    /// Builds the concrete CLI invocation for `definition`: program, args,
    /// this tool's own system-prompt flag (fed `definition.system_prompt`),
    /// and this tool's own model flag when `definition.model` is set.
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand>;

    /// Environment variable names (beyond `PATH`, which
    /// [`crate::process::spawn`] always forwards) this tool needs to find
    /// its own configuration/auth. Never the whole environment -- see
    /// [`crate::process::spawn`]'s own docs for why this remains a named
    /// allowlist rather than `env_clear()` being dropped altogether.
    fn env_allowlist(&self) -> &'static [&'static str];

    /// Transforms one reviewer/tester invocation's raw captured stdout into
    /// the findings it reported (issue #24 point 1, third bullet): the
    /// adapter's job, not a wrapper script the user has to write. Wraps
    /// `warden_core::parse_findings` after stripping whatever envelope this
    /// tool's own output format wraps the agent's final answer in --
    /// **never called for the coder role**, which is judged by exit code
    /// alone (unchanged since ADR-0012).
    fn extract_findings(&self, stdout: &str) -> warden_core::Result<Vec<Finding>>;

    /// The system prompt a role runs with when the base repo has no
    /// `.warden/agents/<role>.md` (issue #24 point 3): what lets
    /// `warden run --tool claude` work with zero markdown at all.
    fn default_prompt(&self, role: AgentRole) -> &'static str;

    /// The `tools` a role runs with when the base repo has no
    /// `.warden/agents/<role>.md` -- the other half of the "zero markdown"
    /// UX alongside [`default_prompt`](ToolAdapter::default_prompt): a role
    /// whose default prompt asks it to act, but whose default `tools` grants
    /// nothing, cannot actually act at all for a tool like `claude` where
    /// tool permission is opt-in even in non-interactive mode (see
    /// `ClaudeAdapter`'s own docs on this). `None` is a legitimate answer
    /// for a tool that needs no such grant.
    fn default_tools(&self, role: AgentRole) -> Option<&'static str>;
}

/// The `claude` adapter (issue #24): Warden's first built-in
/// [`ToolAdapter`], wrapping the `claude` CLI (Claude Code) in non-interactive
/// print mode.
///
/// # Invocation shape
///
/// `claude -p --output-format json --append-system-prompt <prompt> [--model
/// <model>] [--allowedTools <tools>]`, with the role's own context (run
/// intent, or target commit/diff/prior findings, `AgentInputMessage` --
/// unchanged from ADR-0012) still fed on stdin as the user turn: `claude -p`
/// with no positional prompt argument reads its user prompt from stdin in
/// plain-text input mode (verified directly against the real CLI), so this
/// adapter needs no involvement in *that* channel at all -- it only owns the
/// invocation's argv.
///
/// # `--append-system-prompt` reverses ADR-0013 / Q2's "no argv" stance
///
/// ADR-0012/ADR-0013 rejected argv as a channel for a warden-managed system
/// prompt (arbitrary, potentially multi-line text leaking into `ps`/logs).
/// That objection doesn't disappear here -- it is knowingly accepted for
/// this one, tool-specific channel: `claude` has no way to accept a system
/// prompt other than `--append-system-prompt` (its stdin, in text input
/// mode, *is* the user turn; there's no separate system-prompt channel on
/// it), so an adapter that builds "the real CLI invocation" for `claude`
/// has no alternative but to pass it as an argument. This is exactly the
/// trade issue #24 asks for by name ("construit l'invocation réelle du CLI
/// ... `--append-system-prompt` ... depuis le frontmatter"), scoped to this
/// concrete adapter rather than reopening the general warden-native stdin
/// contract (`warden_core::agent_wire`, unchanged by this issue).
///
/// # `--output-format json`
///
/// Lets [`ClaudeAdapter::extract_findings`] parse a single well-formed JSON
/// envelope (`{"type":"result", ..., "result": "<final answer text>"}`,
/// verified directly against the real CLI) instead of scraping
/// human-oriented `text`/`stream-json` output. The default per-role prompts
/// ([`ClaudeAdapter::default_prompt`]) instruct the reviewer/tester to make
/// that final answer *be* NDJSON findings, so `extract_findings` only has to
/// unwrap the envelope before handing the inner text to
/// `warden_core::parse_findings` -- it does not itself understand findings.
///
/// # `--allowedTools` is required for a non-interactive invocation to act at
/// all
///
/// Verified directly against the real CLI (`warden run --tool claude` with
/// no `.warden/agents/coder.md` at all, real repo, real API): a
/// non-interactive `claude -p` invocation with **no** `--allowedTools`
/// denies every mutating tool call (`Write`, `Bash`, ...) outright --
/// `permission_denials` in its own JSON envelope, the agent left unable to
/// do anything but explain that it lacks permission. This holds even though
/// Claude Code's own subagent docs describe an *omitted* `tools:` key as
/// "inherits all tools": that description is about which tools an
/// interactive subagent may be *asked* to use, not about a non-interactive
/// invocation being pre-approved to actually use them without either
/// `--allowedTools` or `--dangerously-skip-permissions` (the latter not
/// used here -- see `ClaudeAdapter::default_tools`'s own docs for why an
/// explicit grant is the chosen fix instead). A `.warden/agents/<role>.md`
/// naming its own `tools:` key is unaffected by any of this and reaches
/// [`ClaudeAdapter::build_command`] exactly as written.
pub struct ClaudeAdapter;

/// `claude`'s own JSON envelope for `--output-format json` (verified
/// directly against a real `claude -p --output-format json` invocation --
/// not documented CLI output, so only the two fields this adapter actually
/// needs are modelled; every other field the real CLI emits is ignored by
/// `serde`'s default "extra fields are fine" behaviour, not
/// `deny_unknown_fields` -- this is *not* a Warden-owned wire contract like
/// `agent_wire`/`agent_def`, it's a third party's output format Warden has
/// no say over and must tolerate changing underneath it).
#[derive(Debug, serde::Deserialize)]
struct ClaudeResultEnvelope {
    /// The agent's final answer text. Absent on some non-success `subtype`s
    /// (e.g. a turn-limit abort) -- modelled as `Option` rather than
    /// defaulted to `""`, so that case is reported as the malformed/unusable
    /// output it is instead of being silently treated as "zero findings".
    result: Option<String>,
}

/// Env vars `claude` needs beyond `PATH` to find its own configuration and
/// credentials (`~/.claude/...`) -- ADR-0005: Warden delegates entirely to
/// the CLI's own already-authenticated state, never handling API keys
/// itself, so the CLI must be able to find where it stashed that state.
///
/// **`USER` is required alongside `HOME`**, discovered by live
/// verification (`warden run --tool claude` against a real repo, real
/// account) rather than assumed from documentation: with only `HOME`
/// forwarded, `claude` reported `"Not logged in · Please run /login"` even
/// though the exact same invocation succeeded outside Warden -- bisected by
/// reproducing the failure directly (`env -i PATH=... HOME=... claude ...`)
/// and adding candidate variables back one at a time until it started
/// succeeding again. On this platform, `claude`'s OAuth credential
/// resolution goes through the OS keychain, which is apparently keyed off
/// the OS user identity (`$USER`), not merely `$HOME`; `$LOGNAME` alone was
/// verified *not* sufficient. Whether this generalizes to every platform
/// `claude` runs on isn't verified here -- `USER` is cheap to forward and
/// carries no secret, so it is included unconditionally rather than gated
/// behind a platform check.
///
/// This is the Architecture.md §10 relaxation issue #24 asks for by name: a
/// documented, minimal, per-adapter allowlist, **not** a switch to
/// inheriting the full environment (`env_clear()` still runs first in
/// [`crate::process::spawn`] -- only these named variables are layered back
/// on top, on an explicit opt-in basis per tool).
const CLAUDE_ENV_ALLOWLIST: &[&str] = &["HOME", "USER"];

impl ToolAdapter for ClaudeAdapter {
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand> {
        let mut args = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
            "--append-system-prompt".to_string(),
            definition.system_prompt.clone(),
        ];
        if let Some(model) = &definition.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        if let Some(tools) = &definition.tools {
            args.push("--allowedTools".to_string());
            args.push(tools.clone());
        }
        Ok(AgentCommand::new("claude", args))
    }

    fn env_allowlist(&self) -> &'static [&'static str] {
        CLAUDE_ENV_ALLOWLIST
    }

    fn extract_findings(&self, stdout: &str) -> warden_core::Result<Vec<Finding>> {
        let envelope: ClaudeResultEnvelope = serde_json::from_str(stdout).map_err(|error| {
            warden_core::CoreError::MalformedAgentOutput(format!(
                "claude output is not the expected --output-format json envelope: {error}"
            ))
        })?;
        let result_text = envelope.result.ok_or_else(|| {
            warden_core::CoreError::MalformedAgentOutput(
                "claude output envelope has no `result` field (the agent likely did not \
                 complete normally)"
                    .to_string(),
            )
        })?;
        warden_core::parse_findings(&result_text)
    }

    fn default_prompt(&self, role: AgentRole) -> &'static str {
        match role {
            AgentRole::Coder => DEFAULT_CODER_PROMPT,
            AgentRole::Reviewer => DEFAULT_REVIEWER_PROMPT,
            AgentRole::Tester => DEFAULT_TESTER_PROMPT,
        }
    }

    fn default_tools(&self, role: AgentRole) -> Option<&'static str> {
        Some(match role {
            // Full implementation capability -- the default coder prompt
            // asks it to implement the intent and commit locally.
            AgentRole::Coder => "Read, Write, Edit, Bash",
            // Read-only + Bash (to run linters/static analysis) --
            // deliberately no Write/Edit: Architecture.md §1 defines the
            // reviewer as raising findings, never fixing them itself, and a
            // default grant should not contradict that division of labour.
            AgentRole::Reviewer => "Read, Grep, Glob, Bash",
            // The default tester prompt asks it to both run the existing
            // test suite (Bash) and add tests the diff lacks (Write/Edit).
            AgentRole::Tester => "Read, Write, Edit, Grep, Glob, Bash",
        })
    }
}

/// Every default prompt tells the agent about the stdin JSON contract
/// (`warden_core::AgentInputMessage`, ADR-0012, unchanged by issue #24) --
/// that channel is not part of `ClaudeAdapter`'s own invocation, but an
/// agent that doesn't know to look for it would never see its intent/
/// findings/diff at all.
const DEFAULT_CODER_PROMPT: &str = "You are Warden's coder agent.\n\n\
Warden will send a single JSON object on stdin (fields: version, role, \
intent, findings) before closing stdin. Read it before doing anything else. \
`intent` is the task to implement or fix on the current branch; `findings` \
(if non-empty) are blocking issues a prior reviewer/tester/CI raised against \
your last attempt -- fix all of them.\n\n\
Implement the change directly in this working tree and commit it locally \
with git before exiting. Do not push this commit anywhere and do not open or \
interact with any pull request -- pushing (and opening a PR) happens later, \
only after this run converges, and is gated separately from this \
invocation.";

const DEFAULT_REVIEWER_PROMPT: &str = "You are Warden's reviewer agent.\n\n\
Warden will send a single JSON object on stdin (fields: version, role, \
target_commit, diff, findings) before closing stdin. Read it before doing \
anything else. Review `diff` (already applied at `target_commit` in this \
working tree) for correctness, security, and implementation issues against \
the intent visible in the commit history; `findings` (if non-empty) lists \
issues from a prior cycle you can check were actually resolved.\n\n\
Your final answer must be nothing but zero or more NDJSON lines (one JSON \
object per line, no wrapping array/object, blank lines ignored), each with \
exactly these fields: `source` (always the string \"reviewer\"), `severity` \
(\"blocking\", \"warning\", or \"info\"), `file` (string or null), \
`description` (string), `action` (string or null). No findings at all means \
no lines. Do not include any other text in your final answer.";

const DEFAULT_TESTER_PROMPT: &str = "You are Warden's tester agent.\n\n\
Warden will send a single JSON object on stdin (fields: version, role, \
target_commit, diff, findings) before closing stdin. Read it before doing \
anything else. Run this project's test suite (and add tests covering `diff`, \
already applied at `target_commit` in this working tree, if it lacks \
coverage) against the intent visible in the commit history; `findings` (if \
non-empty) lists issues from a prior cycle you can check were actually \
resolved.\n\n\
Your final answer must be nothing but zero or more NDJSON lines (one JSON \
object per line, no wrapping array/object, blank lines ignored), each with \
exactly these fields: `source` (always the string \"tester\"), `severity` \
(\"blocking\", \"warning\", or \"info\"), `file` (string or null), \
`description` (string), `action` (string or null). A blocking finding means \
the test suite failed; no findings at all means it passed. Do not include \
any other text in your final answer.";

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(model: Option<&str>, tools: Option<&str>) -> AgentDefinition {
        AgentDefinition::new(
            None,
            None,
            tools.map(str::to_string),
            model.map(str::to_string),
            "be an agent",
        )
        .unwrap()
    }

    #[test]
    fn build_command_always_runs_claude_in_print_json_mode_with_the_system_prompt() {
        let command = ClaudeAdapter
            .build_command(&definition(None, None))
            .unwrap();
        assert_eq!(command.program, "claude");
        assert_eq!(
            command.args,
            vec![
                "-p",
                "--output-format",
                "json",
                "--append-system-prompt",
                "be an agent"
            ]
        );
    }

    #[test]
    fn build_command_appends_model_when_the_definition_sets_one() {
        let command = ClaudeAdapter
            .build_command(&definition(Some("sonnet"), None))
            .unwrap();
        assert!(command.args.windows(2).any(|w| w == ["--model", "sonnet"]));
    }

    #[test]
    fn build_command_appends_allowed_tools_when_the_definition_sets_some() {
        let command = ClaudeAdapter
            .build_command(&definition(None, Some("Read, Edit, Bash")))
            .unwrap();
        assert!(command
            .args
            .windows(2)
            .any(|w| w == ["--allowedTools", "Read, Edit, Bash"]));
    }

    #[test]
    fn build_command_omits_model_and_allowed_tools_flags_when_the_definition_sets_neither() {
        let command = ClaudeAdapter
            .build_command(&definition(None, None))
            .unwrap();
        assert!(!command.args.iter().any(|arg| arg == "--model"));
        assert!(!command.args.iter().any(|arg| arg == "--allowedTools"));
    }

    /// The system prompt is not itself a marker to search for blindly; this
    /// just pins that it rides directly after `--append-system-prompt`,
    /// never split or mangled.
    #[test]
    fn the_system_prompt_is_passed_to_append_system_prompt_intact() {
        let command = ClaudeAdapter
            .build_command(
                &AgentDefinition::new(None, None, None, None, "multi\nline\nprompt").unwrap(),
            )
            .unwrap();
        let flag_index = command
            .args
            .iter()
            .position(|arg| arg == "--append-system-prompt")
            .unwrap();
        assert_eq!(command.args[flag_index + 1], "multi\nline\nprompt");
    }

    #[test]
    fn env_allowlist_is_exactly_home_and_user() {
        assert_eq!(ClaudeAdapter.env_allowlist(), &["HOME", "USER"]);
    }

    #[test]
    fn extract_findings_unwraps_the_result_envelope_and_parses_ndjson_findings() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"source\":\"reviewer\",\"severity\":\"blocking\",\"description\":\"bug\"}"}"#;
        let findings = ClaudeAdapter.extract_findings(stdout).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].description, "bug");
    }

    #[test]
    fn extract_findings_treats_an_empty_result_as_no_findings() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"result":""}"#;
        assert_eq!(ClaudeAdapter.extract_findings(stdout).unwrap(), Vec::new());
    }

    #[test]
    fn extract_findings_rejects_output_that_is_not_the_envelope_json() {
        let error = ClaudeAdapter
            .extract_findings("not json at all")
            .unwrap_err();
        assert!(matches!(
            error,
            warden_core::CoreError::MalformedAgentOutput(_)
        ));
    }

    #[test]
    fn extract_findings_rejects_an_envelope_with_no_result_field() {
        let stdout = r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#;
        let error = ClaudeAdapter.extract_findings(stdout).unwrap_err();
        assert!(matches!(
            error,
            warden_core::CoreError::MalformedAgentOutput(_)
        ));
    }

    #[test]
    fn extract_findings_propagates_the_inner_ndjson_parse_error_for_malformed_findings() {
        let stdout =
            r#"{"type":"result","subtype":"success","is_error":false,"result":"not ndjson"}"#;
        assert!(ClaudeAdapter.extract_findings(stdout).is_err());
    }

    #[test]
    fn every_role_has_a_non_blank_default_prompt() {
        for role in [AgentRole::Coder, AgentRole::Reviewer, AgentRole::Tester] {
            assert!(!ClaudeAdapter.default_prompt(role).trim().is_empty());
        }
    }

    /// The "zero .md" UX must actually be able to act: a `None` default here
    /// would leave every mutating tool call denied in non-interactive mode
    /// (see this module's own docs).
    #[test]
    fn every_role_has_a_non_blank_default_tools_grant() {
        for role in [AgentRole::Coder, AgentRole::Reviewer, AgentRole::Tester] {
            let tools = ClaudeAdapter
                .default_tools(role)
                .expect("every role must have a default tools grant");
            assert!(!tools.trim().is_empty());
        }
    }

    #[test]
    fn the_reviewer_default_tools_grant_excludes_write_and_edit() {
        let tools = ClaudeAdapter.default_tools(AgentRole::Reviewer).unwrap();
        assert!(!tools.contains("Write"), "{tools:?}");
        assert!(!tools.contains("Edit"), "{tools:?}");
    }

    #[test]
    fn the_coder_and_tester_default_tools_grants_include_write_and_edit() {
        for role in [AgentRole::Coder, AgentRole::Tester] {
            let tools = ClaudeAdapter.default_tools(role).unwrap();
            assert!(tools.contains("Write"), "{role:?}: {tools:?}");
            assert!(tools.contains("Edit"), "{role:?}: {tools:?}");
        }
    }
}
