//! The **tool adapter seam** (issue #24): maps a parsed markdown agent
//! definition (`warden_core::AgentDefinition`) onto the concrete subprocess
//! invocation `warden_sandbox::Sandbox::execute` runs (issue #50; strict
//! parity with what used to be `crate::process::spawn` for this path), one
//! built-in CLI at a time, selected once per run by `--tool <name>`.
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
//! specific CLIs (`claude` first; `codex` and `mistral` followed in issue
//! #71; other CLIs are meant to gain their own [`ToolAdapter`] impl later,
//! never a config-declared registry --
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
//!   this tool needs to run at all -- see `warden_sandbox::LocalSandbox`'s
//!   own docs for the Architecture.md §10 relaxation this requires (issue
//!   #50: that forwarding now happens in `warden_sandbox`, not here).
//! - [`ToolAdapter::extract_findings`] turns a reviewer/tester invocation's
//!   raw captured stdout into the findings it reported, so a user never
//!   writes an output-translation wrapper again.
//! - [`ToolAdapter::default_prompt`] gives every role something to run even
//!   when the run's base repo has no `.warden/agents/<role>.md` at all
//!   (`warden::agent_def::resolve_agent_definition`) -- the
//!   `warden run --repo ... --intent ... --tool claude` zero-`.md` UX issue
//!   #24 exists to enable.

use warden_core::{AgentDefinition, AgentRole, Finding, TokenUsage};

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
///
/// `Sync` supertrait (issue #33): `Orchestrator::run_agent` closes over a
/// `&R` inside the `on_stdout_line` callback it hands to
/// `warden_sandbox::Sandbox::execute` (issue #50: this used to be
/// `process::wait_with_progress`), which itself must be `Send + Sync` so
/// its future stays spawnable from any caller (including one that runs it
/// inside `tokio::spawn`, as some tests do). Every real and test implementor
/// here is a stateless unit/plain struct, so this is free in practice, not a
/// constraint that costs an adapter author anything.
pub trait ToolAdapter: Sync {
    /// Builds the concrete CLI invocation for `definition`: program, args,
    /// this tool's own system-prompt flag (fed `definition.system_prompt`),
    /// and this tool's own model flag when `definition.model` is set.
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand>;

    /// Environment variable names (beyond `PATH`, which
    /// `warden_sandbox::LocalSandbox::execute` always forwards) this tool
    /// needs to find its own configuration/auth. Never the whole environment
    /// -- see `warden_sandbox::LocalSandbox`'s own docs for why this remains
    /// a named allowlist rather than `env_clear()` being dropped altogether
    /// (issue #50: that forwarding now happens in `warden_sandbox`, not
    /// `crate::process`).
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

    /// Translates one line of an agent's streamed stdout into a short,
    /// human-readable progress description to publish on the Event Bus as a
    /// `warden_core::RunEvent::AgentProgress`, or `None` if the line carries
    /// nothing worth surfacing to a live observer (issue #33).
    ///
    /// **Declarative, not verified**: the returned text is whatever the
    /// agent's own tool CLI *reports* itself doing (a streamed assistant
    /// message, a `tool_use` block, ...), not a checked execution trace --
    /// ADR-0009's evidence keeps that role, and a caller must never present
    /// this as one. This method is exactly the seam that absorbs a tool's
    /// own wire format (e.g. `claude --output-format stream-json`'s NDJSON
    /// event shape): the returned `String` is plain text, so nothing about
    /// that format ever needs to leak past this adapter into
    /// `warden_core`/`warden-tui`.
    ///
    /// Defaults to `None` for every line -- an adapter is not required to
    /// implement streaming progress at all (the same "a legitimate answer"
    /// convention as [`default_tools`](ToolAdapter::default_tools) returning
    /// `None`); a tool that never overrides this simply produces no
    /// progress signal, degrading gracefully to the pre-issue-#33 silence
    /// between `AgentStarted`/`AgentFinished` rather than failing anything.
    fn parse_progress_line(&self, _line: &str) -> Option<String> {
        None
    }

    /// Extracts the token usage this invocation's underlying tool CLI
    /// reported for it (issue #53), from the exact same captured stdout
    /// [`extract_findings`](ToolAdapter::extract_findings) parses -- never a
    /// second read of the stream (this run's own progress/findings/usage
    /// extraction all graft onto the one captured buffer `Orchestrator::run_agent`
    /// already has). `None` is a legitimate answer, exactly like
    /// [`default_tools`](ToolAdapter::default_tools) and
    /// [`parse_progress_line`](ToolAdapter::parse_progress_line) returning
    /// `None`: a tool whose CLI never reports usage at all (or a malformed
    /// invocation this adapter can't make sense of) yields "n/a" to a caller,
    /// never a fabricated zero.
    ///
    /// Defaults to `None` for every adapter that doesn't override it -- usage
    /// extraction is optional per tool (issue #53 scope: "extraction pour
    /// d'autres CLI que claude" is out of scope, the seam is simply ready for
    /// a future adapter to fill in).
    fn extract_usage(&self, _stdout: &str) -> Option<TokenUsage> {
        None
    }
}

/// The `claude` adapter (issue #24): Warden's first built-in
/// [`ToolAdapter`], wrapping the `claude` CLI (Claude Code) in non-interactive
/// print mode.
///
/// # Invocation shape
///
/// `claude -p --output-format stream-json --verbose --append-system-prompt
/// <prompt> [--model <model>] [--allowedTools <tools>]`, with the role's own
/// context (run intent, or target commit/diff/prior findings,
/// `AgentInputMessage` -- unchanged from ADR-0012) still fed on stdin as the
/// user turn: `claude -p` with no positional prompt argument reads its user
/// prompt from stdin in plain-text input mode (verified directly against the
/// real CLI), so this adapter needs no involvement in *that* channel at all
/// -- it only owns the invocation's argv.
///
/// `--verbose` is not optional decoration: verified directly against the
/// real CLI, `claude -p --output-format stream-json` without it refuses to
/// run at all (`Error: When using --print, --output-format=stream-json
/// requires --verbose`).
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
/// # `--output-format stream-json` (issue #33, was `json`)
///
/// Originally `--output-format json`, chosen specifically because it emits a
/// single well-formed JSON envelope and nothing else on stdout -- see this
/// module's git history for that reasoning. Issue #33 (the TUI is blind for
/// an agent's whole running time -- no signal at all between `AgentStarted`
/// and `AgentFinished`) needed a second consumer of this same output: not
/// just "one parsable final answer" (`extract_findings`) but also "an
/// observable stream of what the agent is doing right now"
/// (`parse_progress_line`). `stream-json` (with `--verbose`, see above)
/// satisfies both without arbitration between them, because it emits its own
/// NDJSON events *as the agent works* (`assistant` messages, `tool_use`
/// blocks) and, verified directly against the real CLI, still ends with the
/// **exact same** `{"type":"result", ..., "result": "<final answer
/// text>"}` envelope as `json` mode did, as its last line. `extract_findings`
/// below therefore doesn't need to change what it looks for, only *where*
/// in stdout it looks (the last non-blank line, not the whole buffer as a
/// single JSON value) -- see that method's own docs.
///
/// Every other line in the stream is `parse_progress_line`'s to interpret;
/// this is a strictly additive change to what this adapter observes, not a
/// new capability for Warden to *act* on anything (ADR-0005 is unaffected --
/// see this issue's own analysis: Warden reads what `claude` already emits,
/// it does not become a tool provider or intercept any command). The default
/// per-role prompts ([`ClaudeAdapter::default_prompt`]) instruct the
/// reviewer/tester to make the final answer *be* NDJSON findings, so
/// `extract_findings` only has to unwrap the envelope before handing the
/// inner text to `warden_core::parse_findings` -- it does not itself
/// understand findings.
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

/// `claude`'s own `result` envelope -- the same shape under both
/// `--output-format json` (the entirety of stdout) and, since issue #33,
/// `--output-format stream-json` (the last NDJSON line of stdout; every
/// earlier line is some other `"type"`, left to [`ClaudeAdapter::parse_progress_line`]).
/// Verified directly against the real CLI in both modes -- not documented
/// CLI output, so only the fields this adapter actually needs are modelled;
/// every other field the real CLI emits is ignored by `serde`'s default
/// "extra fields are fine" behaviour, not `deny_unknown_fields` -- this is
/// *not* a Warden-owned wire contract like `agent_wire`/`agent_def`, it's a
/// third party's output format Warden has no say over and must tolerate
/// changing underneath it).
#[derive(Debug, serde::Deserialize)]
struct ClaudeResultEnvelope {
    /// The agent's final answer text. Absent on some non-success `subtype`s
    /// (e.g. a turn-limit abort) -- modelled as `Option` rather than
    /// defaulted to `""`, so that case is reported as the malformed/unusable
    /// output it is instead of being silently treated as "zero findings".
    result: Option<String>,
    /// Issue #53: this invocation's token usage, cumulative for the whole
    /// turn -- absent entirely on some non-success `subtype`s, same as
    /// `result` above, and modelled as `Option` for exactly that reason
    /// (never defaulted to a zeroed [`ClaudeUsage`], which would misreport
    /// "unknown" as "zero", see `crate::tool_adapter::ClaudeAdapter::extract_usage`).
    #[serde(default)]
    usage: Option<ClaudeUsage>,
}

/// `claude`'s own `usage` object, nested in [`ClaudeResultEnvelope`] --
/// verified directly against the real CLI (see that struct's own docs on
/// this not being a documented, stable wire contract). `cache_read_input_tokens`/
/// `cache_creation_input_tokens` are independently optional: a turn that
/// never engages prompt caching at all is not the same fact as "0 tokens
/// cached", so both are modelled as `Option` rather than defaulted to `0`
/// (`warden_core::TokenUsage`'s own docs make the same distinction).
#[derive(Debug, serde::Deserialize)]
struct ClaudeUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
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
/// `warden_sandbox::LocalSandbox::execute` -- issue #50: that's where this
/// runs now, not `crate::process::spawn` -- only these named variables are
/// layered back on top, on an explicit opt-in basis per tool).
const CLAUDE_ENV_ALLOWLIST: &[&str] = &["HOME", "USER"];

impl ToolAdapter for ClaudeAdapter {
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand> {
        let mut args = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            // Required alongside `stream-json` in print mode (see this
            // struct's own docs) -- without it the CLI refuses to start at
            // all, before ever reaching `--append-system-prompt`.
            "--verbose".to_string(),
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
        // Issue #33: `--output-format stream-json` emits one NDJSON event
        // per line while the agent works, then the exact same `result`
        // envelope `json` mode used to emit as the *entirety* of stdout --
        // now as its last line (verified directly against the real CLI, see
        // `ClaudeAdapter`'s own docs). Every earlier line is
        // `parse_progress_line`'s to interpret, not this method's --
        // finding the last non-blank line first keeps this unchanged for
        // plain `json`-mode stdout too (a single line, trivially "the last
        // one"), so no separate code path is needed for the two modes.
        let last_line = stdout
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .ok_or_else(|| {
                warden_core::CoreError::MalformedAgentOutput(
                    "claude produced no output at all (expected at least a final `result` line)"
                        .to_string(),
                )
            })?;
        let envelope: ClaudeResultEnvelope = serde_json::from_str(last_line).map_err(|error| {
            warden_core::CoreError::MalformedAgentOutput(format!(
                "claude's final output line is not the expected `result` envelope: {error}"
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

    fn parse_progress_line(&self, line: &str) -> Option<String> {
        // Every other event `type` this stream can emit (`system`, `user`,
        // `result`, `rate_limit_event`, ...) is either plumbing/noise for a
        // live observer or already `extract_findings`'s own concern -- only
        // `assistant` messages carry what the ticket asks for: complete
        // messages and `tool_use` blocks, deliberately *not*
        // `--include-partial-messages` chunks (issue #33: "probably too
        // granular ... to validate by implementing", and this adapter never
        // passes that flag in the first place, see `build_command`).
        let parsed: ClaudeStreamLine = serde_json::from_str(line).ok()?;
        if parsed.kind != "assistant" {
            return None;
        }
        let blocks = parsed.message?.content?;
        let parts: Vec<String> = blocks
            .iter()
            .filter_map(|block| match block {
                ClaudeContentBlock::Text { text } => {
                    Some(format!("message: {}", summarize_progress_text(text)))
                }
                ClaudeContentBlock::ToolUse { name, input } => {
                    Some(format_tool_use_progress(name, input))
                }
                ClaudeContentBlock::Other => None,
            })
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" | "))
        }
    }

    /// Issue #53: reads the exact same last non-blank stdout line
    /// [`extract_findings`](ClaudeAdapter::extract_findings) unwraps its
    /// envelope from -- no second read of the stream, just a second, more
    /// tolerant parse of the buffer already captured for this invocation.
    /// Deliberately infallible (`Option`, not `warden_core::Result`) unlike
    /// `extract_findings`: usage is observability, not something a caller
    /// must act on, so a missing/malformed line yields "n/a" (`None`)
    /// rather than failing the whole invocation over a CLI's own output
    /// this adapter can't parse -- see [`ToolAdapter::extract_usage`]'s own
    /// docs on why `None` is a legitimate answer, not a swallowed error.
    fn extract_usage(&self, stdout: &str) -> Option<TokenUsage> {
        let last_line = stdout.lines().rev().find(|line| !line.trim().is_empty())?;
        let envelope: ClaudeResultEnvelope = serde_json::from_str(last_line).ok()?;
        let usage = envelope.usage?;
        Some(TokenUsage::new(
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_input_tokens,
            usage.cache_creation_input_tokens,
        ))
    }
}

/// One line of `claude --output-format stream-json` output, kept
/// deliberately minimal (see `ClaudeResultEnvelope`'s own docs on why this
/// isn't a `deny_unknown_fields` wire contract): only enough structure to
/// recognize an `assistant` message and read its content blocks.
#[derive(Debug, serde::Deserialize)]
struct ClaudeStreamLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: Option<ClaudeStreamMessage>,
}

#[derive(Debug, serde::Deserialize)]
struct ClaudeStreamMessage {
    #[serde(default)]
    content: Option<Vec<ClaudeContentBlock>>,
}

/// One content block of an `assistant` message. `#[serde(other)]` on
/// `Other` means every block type this adapter doesn't specifically
/// translate into progress (`thinking`, `image`, a future addition to the
/// CLI's own format, ...) parses without error and is simply skipped by
/// [`ClaudeAdapter::parse_progress_line`], rather than failing the whole
/// line -- consistent with `code-standards.md`'s "never trust agent output"
/// while still tolerating a third party's format evolving underneath this
/// adapter (see `ClaudeResultEnvelope`'s own docs on the same point).
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClaudeContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

/// How much of a progress line's text is worth showing a live observer --
/// long enough to be useful, short enough that one busy assistant message
/// doesn't dominate the TUI's scrollable event log. Collapses internal
/// whitespace (including newlines) to single spaces first: this is a
/// one-line log entry, not a rendered markdown block.
const MAX_PROGRESS_DETAIL_CHARS: usize = 200;

fn summarize_progress_text(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_PROGRESS_DETAIL_CHARS {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(MAX_PROGRESS_DETAIL_CHARS).collect();
        format!("{truncated}…")
    }
}

/// Field names checked, in order, for a short human-readable summary of a
/// `tool_use` block's `input` (issue #33: "tool_use blocks are likely enough
/// ... start without partial messages"). Not exhaustive over every tool
/// `claude` might call -- a tool whose input uses none of these field names
/// still gets *a* progress line (just the tool name alone, see
/// `format_tool_use_progress`), never a missing/failed one; this is
/// observability, not a schema Warden owns or validates against.
const TOOL_USE_SUMMARY_FIELDS: [&str; 5] =
    ["command", "description", "file_path", "path", "pattern"];

fn format_tool_use_progress(name: &str, input: &serde_json::Value) -> String {
    let summary = TOOL_USE_SUMMARY_FIELDS
        .iter()
        .find_map(|field| input.get(field).and_then(|value| value.as_str()));
    match summary {
        Some(summary) => format!("tool_use: {name} ({})", summarize_progress_text(summary)),
        None => format!("tool_use: {name}"),
    }
}

/// Every default prompt tells the agent about the stdin JSON contract
/// (`warden_core::AgentInputMessage`, ADR-0012, unchanged by issue #24) --
/// that channel is not part of `ClaudeAdapter`'s own invocation, but an
/// agent that doesn't know to look for it would never see its intent/
/// findings/diff at all.
const DEFAULT_CODER_PROMPT: &str = "You are Warden's coder agent.\n\n\
Warden will send a single JSON object on stdin (fields: version, role, \
intent, findings, scope) before closing stdin. Read it before doing \
anything else. `intent` is the task to implement or fix on the current \
branch; `findings` (if non-empty) are blocking issues a prior \
reviewer/tester/CI raised against your last attempt -- fix all of them; \
`scope` is always \"full\" for you (it only ever varies for the reviewer).\n\n\
Implement the change directly in this working tree and commit it locally \
with git before exiting. Do not push this commit anywhere and do not open or \
interact with any pull request -- pushing (and opening a PR) happens later, \
only after this run converges, and is gated separately from this \
invocation.";

const DEFAULT_REVIEWER_PROMPT: &str = "You are Warden's reviewer agent.\n\n\
Warden will send a single JSON object on stdin (fields: version, role, \
target_commit, diff, findings, scope) before closing stdin. Read it before \
doing anything else. `scope` is either \"full\" -- review `diff` (already \
applied at `target_commit` in this working tree) for correctness, security, \
and implementation issues against the intent visible in the commit history, \
same as always -- or \"correctif\": in that mode, `diff` is not this cycle's \
whole change, it is a single fix a coder just made in response to specific \
findings, and `findings` lists exactly those findings, not every issue from \
a prior cycle. When `scope` is \"correctif\", review only that fix against \
the findings it was meant to resolve -- do not re-review anything outside \
`diff`, and do not raise new issues unrelated to those findings. When \
`scope` is \"full\", `findings` (if non-empty) lists issues from a prior \
cycle you can check were actually resolved.\n\n\
Your final answer must be nothing but zero or more NDJSON lines (one JSON \
object per line, no wrapping array/object, blank lines ignored), each with \
exactly these fields: `source` (always the string \"reviewer\"), `severity` \
(\"blocking\", \"warning\", or \"info\"), `file` (string or null), \
`description` (string), `action` (string or null). No findings at all means \
no lines. Do not include any other text in your final answer.";

const DEFAULT_TESTER_PROMPT: &str = "You are Warden's tester agent.\n\n\
Warden will send a single JSON object on stdin (fields: version, role, \
target_commit, diff, findings, scope) before closing stdin. Read it before \
doing anything else. Run this project's test suite (and add tests covering \
`diff`, already applied at `target_commit` in this working tree, if it \
lacks coverage) against the intent visible in the commit history; `findings` \
(if non-empty) lists issues from a prior cycle you can check were actually \
resolved. `scope` is always \"full\" for you (it only ever varies for the \
reviewer).\n\n\
Your final answer must be nothing but zero or more NDJSON lines (one JSON \
object per line, no wrapping array/object, blank lines ignored), each with \
exactly these fields: `source` (always the string \"tester\"), `severity` \
(\"blocking\", \"warning\", or \"info\"), `file` (string or null), \
`description` (string), `action` (string or null). A blocking finding means \
the test suite failed; no findings at all means it passed. Do not include \
any other text in your final answer.";

/// The `codex` adapter (issue #71): Warden's second built-in [`ToolAdapter`],
/// wrapping the OpenAI Codex CLI (the `codex` binary) in its non-interactive
/// `codex exec` mode.
///
/// # Open question resolved pragmatically, not verified live (issue #71)
///
/// Unlike [`ClaudeAdapter`], whose every claim above is checked against a
/// real, authenticated CLI, this adapter's invocation shape and JSON event
/// schema were **not** verified against a live `codex` install -- this
/// environment has neither the binary nor network access to install/run it.
/// What follows is this adapter's author's best-effort, documented reading
/// of OpenAI's own published Codex CLI ("codex-rs") behaviour at the time of
/// writing (`codex exec`'s non-interactive mode, its experimental `--json`
/// streamed-event output, and its `--sandbox`/`--ask-for-approval` execution
/// policy flags), not a byte-for-byte captured transcript the way
/// `ClaudeAdapter`'s own regression fixtures are. Should the real CLI's
/// flags or event shape differ from what's modelled here, the failure mode
/// is deliberately graceful rather than silent: `extract_findings` surfaces
/// a named [`warden_core::CoreError::MalformedAgentOutput`] instead of
/// fabricating findings, and `extract_usage` degrades to `None` ("n/a",
/// issue #53) instead of a fabricated zero -- the exact two contracts
/// [`ToolAdapter`]'s own docs already require of every adapter, live-verified
/// or not. Tests below exercise this adapter's own parsing logic against
/// fixtures matching the schema documented here, not a real captured
/// transcript (see this module's own test docs).
///
/// # Invocation shape
///
/// `codex exec --json --ask-for-approval never [--sandbox <mode>] [--model
/// <model>] <system_prompt>`, with the role's own `AgentInputMessage`
/// context still fed on stdin exactly as for every other adapter (unchanged
/// ADR-0012 channel, `Orchestrator::run_agent`). `codex exec`'s own
/// positional `PROMPT` argument reads from stdin when omitted -- the same
/// reason [`ClaudeAdapter`] is forced onto an argv flag for its system
/// prompt rather than stdin (see that struct's own docs): stdin here already
/// carries Warden's own JSON payload, so the system prompt has no channel
/// left but argv, the ADR-0013/Q2 trade-off `ClaudeAdapter` already accepts,
/// scoped to this adapter too.
///
/// - `--json`: the CLI's own experimental streamed-event output mode
///   (this ticket's own research), the `codex` analogue of
///   `claude --output-format stream-json` -- both an `extract_findings`
///   source and a `parse_progress_line` source, same split as
///   `ClaudeAdapter`.
/// - `--ask-for-approval never`: unconditional, never driven by the
///   definition -- a non-interactive subprocess with no human present to
///   answer an approval prompt would otherwise hang forever. This is about
///   *whether a human is asked*, not *what the agent may do*; that's
///   `--sandbox`, below.
/// - `--sandbox <mode>`: fed from `definition.tools` verbatim, the same
///   "reinterpreted per adapter, passed through unvalidated" convention
///   `definition.model` already uses (`AgentDefinition`'s own docs) --
///   `tools:` in a markdown definition means "the comma-separated
///   `--allowedTools` list" for `claude` and "one of `codex`'s own sandbox
///   policy names (`read-only`/`workspace-write`/`danger-full-access`)" for
///   `codex`; a value that isn't one of those is `codex`'s own error to
///   report, not validated here -- see [`CodexAdapter::default_tools`].
pub struct CodexAdapter;

/// Env vars `codex` needs beyond `PATH` to find its own configuration and
/// credentials -- ADR-0005: Warden delegates entirely to the CLI's own
/// already-authenticated state (`codex login`), never handling API keys
/// itself. Only `HOME` is forwarded: unlike `ClaudeAdapter`'s `USER`
/// (empirically required for that CLI's keychain-based OAuth resolution on
/// this platform, see that adapter's own docs), no equivalent requirement is
/// known here -- this has not been verified against a live install (see this
/// module's own docs on `CodexAdapter`), so nothing beyond the one variable
/// this adapter is confident every config-file-based CLI needs is forwarded.
/// Deliberately **not** an `OPENAI_API_KEY`-shaped variable: forwarding a raw
/// API key would make Warden a holder of that secret, contradicting ADR-0005
/// ("Warden holds no keys") -- this adapter relies on `codex`'s own
/// already-authenticated on-disk session, exactly like `ClaudeAdapter`.
const CODEX_ENV_ALLOWLIST: &[&str] = &["HOME"];

impl ToolAdapter for CodexAdapter {
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand> {
        let mut args = vec![
            "exec".to_string(),
            "--json".to_string(),
            "--ask-for-approval".to_string(),
            "never".to_string(),
        ];
        if let Some(tools) = &definition.tools {
            args.push("--sandbox".to_string());
            args.push(tools.clone());
        }
        if let Some(model) = &definition.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        // The positional `PROMPT` argument: `codex exec`'s only channel for
        // a system prompt (see this struct's own docs) -- pushed last, after
        // every flag, matching the CLI's documented `[OPTIONS] [PROMPT]`
        // argument order.
        args.push(definition.system_prompt.clone());
        Ok(AgentCommand::new("codex", args))
    }

    fn env_allowlist(&self) -> &'static [&'static str] {
        CODEX_ENV_ALLOWLIST
    }

    fn extract_findings(&self, stdout: &str) -> warden_core::Result<Vec<Finding>> {
        // Same "last non-blank line carries the terminal envelope" contract
        // `ClaudeAdapter::extract_findings` relies on for its own streamed
        // output -- see this struct's own docs for why `codex exec --json`
        // is modelled the same way (a `task_complete` event as the stream's
        // last line).
        let last_line = stdout
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .ok_or_else(|| {
                warden_core::CoreError::MalformedAgentOutput(
                    "codex produced no output at all (expected at least a final \
                     `task_complete` event)"
                        .to_string(),
                )
            })?;
        let event: CodexEvent = serde_json::from_str(last_line).map_err(|error| {
            warden_core::CoreError::MalformedAgentOutput(format!(
                "codex's final output line is not the expected JSON event envelope: {error}"
            ))
        })?;
        match event.msg {
            CodexEventMsg::TaskComplete {
                last_agent_message: Some(text),
            } => warden_core::parse_findings(&text),
            CodexEventMsg::TaskComplete {
                last_agent_message: None,
            } => Err(warden_core::CoreError::MalformedAgentOutput(
                "codex reported task_complete with no last_agent_message (the agent likely did \
                 not complete normally)"
                    .to_string(),
            )),
            CodexEventMsg::Error { message } => Err(warden_core::CoreError::MalformedAgentOutput(
                format!("codex reported an error: {message}"),
            )),
            CodexEventMsg::AgentMessage { .. } | CodexEventMsg::TokenCount { .. } => {
                Err(warden_core::CoreError::MalformedAgentOutput(
                    "codex's final output line is not a `task_complete` event".to_string(),
                ))
            }
            CodexEventMsg::Other => Err(warden_core::CoreError::MalformedAgentOutput(
                "codex's final output line is an unrecognized event type".to_string(),
            )),
        }
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
            // Full read/write within the role's own worktree -- the default
            // coder prompt asks it to implement the intent and commit
            // locally, same capability `ClaudeAdapter`'s `Bash`/`Write`/`Edit`
            // grant gives its own coder.
            AgentRole::Coder => "workspace-write",
            // Architecture.md §1: the reviewer raises findings, it never
            // fixes them itself -- `read-only` is `codex`'s own sandbox
            // policy closest to `ClaudeAdapter`'s deliberate omission of
            // `Write`/`Edit` from the reviewer's grant.
            AgentRole::Reviewer => "read-only",
            // The default tester prompt asks it to both run the existing
            // suite and add tests the diff lacks, same as
            // `ClaudeAdapter::default_tools`'s tester grant.
            AgentRole::Tester => "workspace-write",
        })
    }

    fn parse_progress_line(&self, line: &str) -> Option<String> {
        // Conservative on purpose (see this struct's own docs): only the one
        // event variant this adapter's author is confident enough to model
        // (`agent_message`, a complete assistant message) is translated into
        // progress text. Every other event type this stream might carry
        // (`task_complete`/`error`, already `extract_findings`'s concern; an
        // unrecognized type; a malformed line) yields `None` rather than a
        // guessed interpretation.
        let event: CodexEvent = serde_json::from_str(line).ok()?;
        match event.msg {
            CodexEventMsg::AgentMessage { message } => {
                Some(format!("message: {}", summarize_progress_text(&message)))
            }
            _ => None,
        }
    }

    /// Issue #53: unlike `ClaudeAdapter`, which reads usage off the exact
    /// same terminal `result` line `extract_findings` unwraps, this adapter
    /// models `codex`'s token usage as its own distinct streamed event
    /// (`token_count`), not necessarily the stream's last line -- so this
    /// scans every captured line, keeping the last `token_count` event seen
    /// (a cumulative report superseding any earlier one), rather than only
    /// the final line. No cache-token dimension is modelled: nothing in this
    /// adapter's documented understanding of `codex`'s protocol (see this
    /// struct's own docs) describes a prompt-caching figure distinct from
    /// input/output tokens, so both cache fields are always `None` here --
    /// never fabricated as `0` (same "n/a, not zero" contract
    /// `ClaudeAdapter::extract_usage`'s own docs describe).
    fn extract_usage(&self, stdout: &str) -> Option<TokenUsage> {
        stdout.lines().rev().find_map(|line| {
            let event: CodexEvent = serde_json::from_str(line).ok()?;
            match event.msg {
                CodexEventMsg::TokenCount {
                    input_tokens,
                    output_tokens,
                } => Some(TokenUsage::new(input_tokens, output_tokens, None, None)),
                _ => None,
            }
        })
    }
}

/// One line of `codex exec --json` output, modelled from OpenAI's published
/// Codex CLI event protocol as this adapter's author understood it -- **not
/// independently verified against a live install** (see [`CodexAdapter`]'s
/// own docs on why). Kept deliberately tolerant, the same convention
/// `ClaudeStreamLine`/`ClaudeResultEnvelope` already use for a third party's
/// own wire format: unknown top-level fields are ignored by serde's default
/// behaviour, and an unrecognized `msg.type` parses into
/// [`CodexEventMsg::Other`] (via `#[serde(other)]`) rather than failing the
/// whole line.
#[derive(Debug, serde::Deserialize)]
struct CodexEvent {
    msg: CodexEventMsg,
}

/// The `msg.type` payload of one [`CodexEvent`] -- see that struct's own
/// docs on this not being a verified, stable wire contract.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexEventMsg {
    /// A complete assistant message -- `parse_progress_line`'s only
    /// recognized event (see [`CodexAdapter::parse_progress_line`]'s own
    /// docs on why this adapter models no other progress-worthy event type).
    AgentMessage { message: String },
    /// The stream's terminal event -- `extract_findings`'s target, the
    /// `codex` analogue of `claude`'s own `result` envelope.
    /// `last_agent_message` absent (rather than defaulted to `""`) reports
    /// the same "the agent likely did not complete normally" fact
    /// `ClaudeResultEnvelope::result`'s own `Option` models.
    TaskComplete {
        #[serde(default)]
        last_agent_message: Option<String>,
    },
    /// Issue #53's source for this adapter -- required, non-`Option` fields:
    /// a `token_count` event missing either figure fails to deserialize
    /// (caught by `extract_usage`'s `.ok()?`) rather than reporting a
    /// fabricated `0` for the missing one.
    TokenCount {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// A terminal failure this invocation reported on its own -- distinct
    /// from a malformed line `extract_findings` itself can't parse at all.
    Error { message: String },
    /// Every event type this adapter doesn't specifically model (a future
    /// addition to the CLI's own protocol, `task_started`,
    /// `exec_command_begin`/`exec_command_end`, ...) -- tolerated, not
    /// treated as this line failing to parse at all, same convention as
    /// `ClaudeContentBlock::Other`.
    #[serde(other)]
    Other,
}

/// The `mistral` adapter (issue #71): Warden's third built-in
/// [`ToolAdapter`], wrapping a `mistral` CLI in the most conservative shape
/// defensible without a live install to verify against.
///
/// # Open question resolved conservatively, not guessed at (issue #71)
///
/// Issue #71 itself flags this CLI's maturity/existence as uncertain (`le
/// chat`/`mistral` -- unlike `codex`, no single, widely-documented
/// non-interactive automation mode could be identified with any confidence
/// while writing this adapter). Per this ticket's own guidance ("prefer a
/// design that degrades gracefully over guessing at a format that may not
/// exist"), this adapter therefore assumes the least possible about the
/// underlying CLI's own contract:
///
/// - No structured/streamed output format is assumed at all --
///   [`extract_usage`](MistralAdapter::extract_usage) always returns `None`
///   ("n/a", issue #53) and [`parse_progress_line`](ToolAdapter::parse_progress_line)
///   is left at the trait's own default (`None` for every line -- issue #33's
///   "legitimate to not implement progress at all", see
///   [`ToolAdapter::parse_progress_line`]'s own docs).
/// - [`extract_findings`](MistralAdapter::extract_findings) treats the
///   **entire trimmed stdout** as the final answer text -- no envelope to
///   unwrap at all, unlike `claude`/`codex`. This is invocation-shape-
///   independent: if the real `mistral` CLI's argv turns out to differ from
///   what [`build_command`](MistralAdapter::build_command) assumes below,
///   only that method needs to change -- this findings contract does not.
///
/// # Invocation shape
///
/// `mistral --system <system_prompt> [--model <model>]` -- the minimal shape
/// a plain prompt-in/answer-out CLI would expose, with the role's own
/// `AgentInputMessage` context still fed on stdin exactly as for every other
/// adapter (unchanged ADR-0012 channel). `definition.tools` is not consumed
/// at all: no equivalent of `claude`'s `--allowedTools`/`codex`'s `--sandbox`
/// is known for this CLI's surface -- see
/// [`MistralAdapter::default_tools`].
pub struct MistralAdapter;

/// Env vars `mistral` needs beyond `PATH` -- ADR-0005: delegated entirely to
/// the CLI's own already-authenticated state, never an API key Warden itself
/// holds (same reasoning as [`CODEX_ENV_ALLOWLIST`]'s own docs). Only `HOME`
/// is forwarded, on the same "every config-file-based CLI needs at least
/// this" reasoning -- not verified against a live install, since this CLI's
/// own configuration/credential storage location is itself unconfirmed (see
/// this module's own docs on [`MistralAdapter`]).
const MISTRAL_ENV_ALLOWLIST: &[&str] = &["HOME"];

impl ToolAdapter for MistralAdapter {
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand> {
        let mut args = vec!["--system".to_string(), definition.system_prompt.clone()];
        if let Some(model) = &definition.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        Ok(AgentCommand::new("mistral", args))
    }

    fn env_allowlist(&self) -> &'static [&'static str] {
        MISTRAL_ENV_ALLOWLIST
    }

    fn extract_findings(&self, stdout: &str) -> warden_core::Result<Vec<Finding>> {
        // No envelope to unwrap (see this adapter's own docs) -- the whole
        // trimmed buffer is the agent's final answer, handed straight to
        // `warden_core::parse_findings` exactly like `ClaudeAdapter`'s
        // original single-line `--output-format json` mode did before
        // `stream-json` introduced an envelope (see `ClaudeAdapter`'s own
        // docs on that history).
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Err(warden_core::CoreError::MalformedAgentOutput(
                "mistral produced no output at all".to_string(),
            ));
        }
        warden_core::parse_findings(trimmed)
    }

    fn default_prompt(&self, role: AgentRole) -> &'static str {
        match role {
            AgentRole::Coder => DEFAULT_CODER_PROMPT,
            AgentRole::Reviewer => DEFAULT_REVIEWER_PROMPT,
            AgentRole::Tester => DEFAULT_TESTER_PROMPT,
        }
    }

    fn default_tools(&self, _role: AgentRole) -> Option<&'static str> {
        // No known equivalent of `--allowedTools`/`--sandbox` for this CLI's
        // surface (see this adapter's own docs) -- `None` is the legitimate
        // answer `ToolAdapter::default_tools`'s own docs describe for a tool
        // that needs no such grant, not an oversight.
        None
    }
}

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
    fn build_command_always_runs_claude_in_print_stream_json_mode_with_the_system_prompt() {
        let command = ClaudeAdapter
            .build_command(&definition(None, None))
            .unwrap();
        assert_eq!(command.program, "claude");
        assert_eq!(
            command.args,
            vec![
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
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

    /// Issue #33: `--output-format stream-json` prints many NDJSON lines
    /// before the final `result` envelope -- `extract_findings` must find
    /// that envelope as the *last* non-blank line, ignoring everything
    /// preceding it, rather than trying to parse the whole buffer as one
    /// JSON value the way `--output-format json` mode's single-line output
    /// allowed.
    #[test]
    fn extract_findings_finds_the_result_envelope_as_the_last_line_of_a_stream_json_transcript() {
        let stdout = concat!(
            r#"{"type":"system","subtype":"init","cwd":"/tmp"}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"file.txt"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"source\":\"reviewer\",\"severity\":\"blocking\",\"description\":\"bug\"}"}"#,
            "\n",
        );
        let findings = ClaudeAdapter.extract_findings(stdout).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].description, "bug");
    }

    /// A trailing blank line (a stray final `\n`) must not be mistaken for
    /// "no output at all", nor parsed as the result line itself.
    #[test]
    fn extract_findings_ignores_a_trailing_blank_line_after_the_result_envelope() {
        let stdout =
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\"}\n\n";
        assert_eq!(ClaudeAdapter.extract_findings(stdout).unwrap(), Vec::new());
    }

    #[test]
    fn extract_findings_rejects_completely_empty_output() {
        let error = ClaudeAdapter.extract_findings("").unwrap_err();
        assert!(matches!(
            error,
            warden_core::CoreError::MalformedAgentOutput(_)
        ));
    }

    #[test]
    fn parse_progress_line_extracts_a_complete_assistant_text_message() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Looking at the failing test now."}]}}"#;
        let progress = ClaudeAdapter.parse_progress_line(line).unwrap();
        assert_eq!(progress, "message: Looking at the failing test now.");
    }

    #[test]
    fn parse_progress_line_extracts_a_tool_use_block_with_its_command() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test","description":"run the suite"}}]}}"#;
        let progress = ClaudeAdapter.parse_progress_line(line).unwrap();
        assert_eq!(progress, "tool_use: Bash (cargo test)");
    }

    #[test]
    fn parse_progress_line_falls_back_to_the_bare_tool_name_when_input_has_no_known_field() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"CustomTool","input":{"weird_field":"value"}}]}}"#;
        let progress = ClaudeAdapter.parse_progress_line(line).unwrap();
        assert_eq!(progress, "tool_use: CustomTool");
    }

    #[test]
    fn parse_progress_line_joins_multiple_content_blocks_in_one_assistant_message() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll list the files."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#;
        let progress = ClaudeAdapter.parse_progress_line(line).unwrap();
        assert_eq!(
            progress,
            "message: I'll list the files. | tool_use: Bash (ls)"
        );
    }

    #[test]
    fn parse_progress_line_ignores_non_assistant_event_types() {
        for line in [
            r#"{"type":"system","subtype":"init","cwd":"/tmp"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":""}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{}}"#,
        ] {
            assert_eq!(ClaudeAdapter.parse_progress_line(line), None, "{line}");
        }
    }

    #[test]
    fn parse_progress_line_returns_none_for_unparsable_or_unrelated_lines() {
        assert_eq!(ClaudeAdapter.parse_progress_line("not json at all"), None);
        assert_eq!(ClaudeAdapter.parse_progress_line(""), None);
    }

    /// Long text is truncated for the live log rather than dumped verbatim
    /// -- one huge assistant message must not dominate the TUI's scrollable
    /// event list.
    #[test]
    fn parse_progress_line_truncates_a_very_long_assistant_message() {
        let long_text = "word ".repeat(100); // well past MAX_PROGRESS_DETAIL_CHARS
        let line = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{long_text}"}}]}}}}"#
        );
        let progress = ClaudeAdapter.parse_progress_line(&line).unwrap();
        assert!(progress.chars().count() < long_text.chars().count());
        assert!(progress.ends_with('…'));
    }

    /// A content block type this adapter doesn't specifically translate
    /// (e.g. `thinking`) must not fail the whole line -- it is simply
    /// skipped, per `ClaudeContentBlock::Other`'s own docs.
    #[test]
    fn parse_progress_line_skips_unrecognized_content_block_types_without_failing() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"pondering..."}]}}"#;
        assert_eq!(ClaudeAdapter.parse_progress_line(line), None);
    }

    /// Regression fixture captured verbatim from a real
    /// `claude -p --output-format stream-json --verbose` invocation (issue
    /// #33 implementation) -- not a hand-written approximation, so it also
    /// exercises fields this adapter doesn't model (`caller`, `usage`,
    /// `parent_tool_use_id`, ...) tolerated by serde's default "extra
    /// fields are fine" behaviour (see `ClaudeStreamLine`'s own docs).
    #[test]
    fn parse_progress_line_handles_a_real_captured_tool_use_line() {
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg_011Cd9HiRpTrNZbpg6SsxFb7","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_01GLvBUCpwy33TmUv2zhwwWc","name":"Bash","input":{"command":"ls -la /private/tmp/stream-json-probe","description":"List files in working dir"},"caller":{"type":"direct"}}],"stop_reason":null,"stop_sequence":null,"stop_details":null,"usage":{"input_tokens":2},"diagnostics":null,"context_management":null},"parent_tool_use_id":null,"session_id":"4d83aff1-794c-4154-8cbf-34a6beb3423a","uuid":"dd65275f-4dc6-402b-bc2b-d7c5bf62f8af","timestamp":"2026-07-18T09:30:28.145Z","request_id":"req_011Cd9HiKv1bhZVTuY3FD96a"}"#;
        let progress = ClaudeAdapter.parse_progress_line(line).unwrap();
        assert_eq!(
            progress,
            "tool_use: Bash (ls -la /private/tmp/stream-json-probe)"
        );
    }

    /// Same real-capture provenance as the test above, for the terminal
    /// `result` line -- an em-dash and embedded newlines in `result` (real
    /// model output, not a test fixture's simplification) must round-trip
    /// through `extract_findings` without corruption.
    #[test]
    fn extract_findings_handles_a_real_captured_result_line() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"api_error_status":null,"duration_ms":8120,"result":"Files:\n- `err.log` — empty\n- `out.ndjson` — 11.7 KB\n\nHello.","stop_reason":"end_turn","session_id":"4d83aff1-794c-4154-8cbf-34a6beb3423a","total_cost_usd":0.1483495,"permission_denials":[],"terminal_reason":"completed","uuid":"1d4ae256-0ea2-4dc3-addd-526a68c08806"}"#;
        // This transcript's final answer is free text (an em-dash and
        // embedded newlines, real model output, not a test simplification),
        // not a reviewer/tester's NDJSON findings -- `extract_findings`
        // must still correctly unwrap the real envelope (proving the
        // `result` field round-trips through serde intact) before failing
        // for the unrelated reason of `parse_findings` rejecting non-NDJSON
        // text.
        let error = match ClaudeAdapter.extract_findings(stdout).unwrap_err() {
            warden_core::CoreError::MalformedAgentOutput(message) => message,
            other => panic!("expected MalformedAgentOutput, got {other:?}"),
        };
        assert!(
            !error.contains("envelope"),
            "must fail on the inner NDJSON, not on unwrapping the envelope: {error}"
        );
    }

    // -----------------------------------------------------------------
    // `extract_usage` (issue #53)
    // -----------------------------------------------------------------

    #[test]
    fn extract_usage_reads_input_output_and_cache_tokens_from_the_result_envelope() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","usage":{"input_tokens":120,"output_tokens":45,"cache_read_input_tokens":10,"cache_creation_input_tokens":3}}"#;
        let usage = ClaudeAdapter.extract_usage(stdout).unwrap();
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 45);
        assert_eq!(usage.cache_read_tokens, Some(10));
        assert_eq!(usage.cache_creation_tokens, Some(3));
    }

    #[test]
    fn extract_usage_tolerates_missing_cache_fields() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","usage":{"input_tokens":120,"output_tokens":45}}"#;
        let usage = ClaudeAdapter.extract_usage(stdout).unwrap();
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 45);
        assert_eq!(usage.cache_read_tokens, None);
        assert_eq!(usage.cache_creation_tokens, None);
    }

    /// Issue #53 scope: a tool CLI that reports no usage at all (or an
    /// invocation this adapter otherwise can't make sense of) must yield
    /// "n/a" (`None`), never a fabricated zero and never a failed invocation
    /// -- `extract_usage` is infallible by design, unlike `extract_findings`.
    #[test]
    fn extract_usage_returns_none_when_the_result_envelope_has_no_usage_field() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#;
        assert_eq!(ClaudeAdapter.extract_usage(stdout), None);
    }

    #[test]
    fn extract_usage_returns_none_for_output_that_is_not_the_envelope_json() {
        assert_eq!(ClaudeAdapter.extract_usage("not json at all"), None);
    }

    #[test]
    fn extract_usage_returns_none_for_completely_empty_output() {
        assert_eq!(ClaudeAdapter.extract_usage(""), None);
    }

    /// Same "last non-blank line, not the whole buffer" contract
    /// `extract_findings` already relies on for `--output-format
    /// stream-json`'s multi-line transcripts (issue #33) -- `extract_usage`
    /// must find the same terminal `result` line, ignoring every earlier
    /// NDJSON event.
    #[test]
    fn extract_usage_finds_the_result_envelope_as_the_last_line_of_a_stream_json_transcript() {
        let stdout = concat!(
            r#"{"type":"system","subtype":"init","cwd":"/tmp"}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","usage":{"input_tokens":7,"output_tokens":2}}"#,
            "\n",
        );
        let usage = ClaudeAdapter.extract_usage(stdout).unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 2);
    }

    /// Regression coverage using the same real-captured `result` line
    /// `extract_findings_handles_a_real_captured_result_line` uses -- that
    /// fixture's real invocation reported no `usage` field at all, so this
    /// pins `extract_usage`'s "n/a, not an error" contract against real CLI
    /// output, not just a hand-written approximation.
    #[test]
    fn extract_usage_returns_none_for_the_real_captured_result_line_with_no_usage_field() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"api_error_status":null,"duration_ms":8120,"result":"Files:\n- `err.log` — empty\n- `out.ndjson` — 11.7 KB\n\nHello.","stop_reason":"end_turn","session_id":"4d83aff1-794c-4154-8cbf-34a6beb3423a","total_cost_usd":0.1483495,"permission_denials":[],"terminal_reason":"completed","uuid":"1d4ae256-0ea2-4dc3-addd-526a68c08806"}"#;
        assert_eq!(ClaudeAdapter.extract_usage(stdout), None);
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

    // ===================================================================
    // `CodexAdapter` (issue #71) -- fixtures below match the JSON event
    // schema documented on `CodexEvent`/`CodexEventMsg`, this adapter's own
    // best-effort reading of `codex exec --json`, not a real captured
    // transcript (see `CodexAdapter`'s own docs on why no such transcript
    // exists here).
    // ===================================================================

    #[test]
    fn codex_build_command_always_execs_in_json_never_ask_mode_with_the_prompt_last() {
        let command = CodexAdapter.build_command(&definition(None, None)).unwrap();
        assert_eq!(command.program, "codex");
        assert_eq!(
            command.args,
            vec![
                "exec",
                "--json",
                "--ask-for-approval",
                "never",
                "be an agent"
            ]
        );
    }

    #[test]
    fn codex_build_command_appends_sandbox_when_the_definition_sets_tools() {
        let command = CodexAdapter
            .build_command(&definition(None, Some("workspace-write")))
            .unwrap();
        assert!(command
            .args
            .windows(2)
            .any(|w| w == ["--sandbox", "workspace-write"]));
    }

    #[test]
    fn codex_build_command_appends_model_when_the_definition_sets_one() {
        let command = CodexAdapter
            .build_command(&definition(Some("o3"), None))
            .unwrap();
        assert!(command.args.windows(2).any(|w| w == ["--model", "o3"]));
    }

    #[test]
    fn codex_build_command_places_the_system_prompt_as_the_trailing_positional_argument() {
        let command = CodexAdapter
            .build_command(&definition(Some("o3"), Some("read-only")))
            .unwrap();
        assert_eq!(command.args.last().unwrap(), "be an agent");
    }

    #[test]
    fn codex_env_allowlist_is_exactly_home() {
        assert_eq!(CodexAdapter.env_allowlist(), &["HOME"]);
    }

    #[test]
    fn codex_extract_findings_unwraps_task_complete_and_parses_ndjson_findings() {
        let stdout = concat!(
            r#"{"msg":{"type":"agent_message","message":"looking into it"}}"#,
            "\n",
            r#"{"msg":{"type":"task_complete","last_agent_message":"{\"source\":\"reviewer\",\"severity\":\"blocking\",\"description\":\"bug\"}"}}"#,
        );
        let findings = CodexAdapter.extract_findings(stdout).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].description, "bug");
    }

    #[test]
    fn codex_extract_findings_treats_an_empty_last_agent_message_as_no_findings() {
        let stdout = r#"{"msg":{"type":"task_complete","last_agent_message":""}}"#;
        assert_eq!(CodexAdapter.extract_findings(stdout).unwrap(), Vec::new());
    }

    #[test]
    fn codex_extract_findings_rejects_a_task_complete_with_no_last_agent_message() {
        let stdout = r#"{"msg":{"type":"task_complete"}}"#;
        let error = CodexAdapter.extract_findings(stdout).unwrap_err();
        assert!(matches!(
            error,
            warden_core::CoreError::MalformedAgentOutput(_)
        ));
    }

    #[test]
    fn codex_extract_findings_surfaces_a_reported_error_event() {
        let stdout = r#"{"msg":{"type":"error","message":"sandbox denied write"}}"#;
        let error = match CodexAdapter.extract_findings(stdout).unwrap_err() {
            warden_core::CoreError::MalformedAgentOutput(message) => message,
            other => panic!("expected MalformedAgentOutput, got {other:?}"),
        };
        assert!(error.contains("sandbox denied write"));
    }

    #[test]
    fn codex_extract_findings_rejects_output_that_is_not_the_event_envelope() {
        let error = CodexAdapter
            .extract_findings("not json at all")
            .unwrap_err();
        assert!(matches!(
            error,
            warden_core::CoreError::MalformedAgentOutput(_)
        ));
    }

    #[test]
    fn codex_extract_findings_rejects_completely_empty_output() {
        let error = CodexAdapter.extract_findings("").unwrap_err();
        assert!(matches!(
            error,
            warden_core::CoreError::MalformedAgentOutput(_)
        ));
    }

    #[test]
    fn codex_extract_findings_finds_the_task_complete_event_as_the_last_line_of_a_transcript() {
        let stdout = concat!(
            r#"{"msg":{"type":"agent_message","message":"reading files"}}"#,
            "\n",
            r#"{"msg":{"type":"agent_message","message":"done reviewing"}}"#,
            "\n",
            r#"{"msg":{"type":"task_complete","last_agent_message":"{\"source\":\"reviewer\",\"severity\":\"blocking\",\"description\":\"bug\"}"}}"#,
            "\n",
        );
        let findings = CodexAdapter.extract_findings(stdout).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].description, "bug");
    }

    #[test]
    fn codex_parse_progress_line_extracts_an_agent_message_event() {
        let line =
            r#"{"msg":{"type":"agent_message","message":"Looking at the failing test now."}}"#;
        let progress = CodexAdapter.parse_progress_line(line).unwrap();
        assert_eq!(progress, "message: Looking at the failing test now.");
    }

    #[test]
    fn codex_parse_progress_line_ignores_non_agent_message_event_types() {
        for line in [
            r#"{"msg":{"type":"task_complete","last_agent_message":""}}"#,
            r#"{"msg":{"type":"token_count","input_tokens":1,"output_tokens":1}}"#,
            r#"{"msg":{"type":"error","message":"boom"}}"#,
            r#"{"msg":{"type":"exec_command_begin","command":"ls"}}"#,
        ] {
            assert_eq!(CodexAdapter.parse_progress_line(line), None, "{line}");
        }
    }

    #[test]
    fn codex_parse_progress_line_returns_none_for_unparsable_lines() {
        assert_eq!(CodexAdapter.parse_progress_line("not json at all"), None);
        assert_eq!(CodexAdapter.parse_progress_line(""), None);
    }

    #[test]
    fn codex_extract_usage_reads_input_and_output_tokens_from_a_token_count_event() {
        let stdout = r#"{"msg":{"type":"token_count","input_tokens":120,"output_tokens":45}}"#;
        let usage = CodexAdapter.extract_usage(stdout).unwrap();
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 45);
        assert_eq!(usage.cache_read_tokens, None);
        assert_eq!(usage.cache_creation_tokens, None);
    }

    #[test]
    fn codex_extract_usage_finds_a_token_count_event_anywhere_in_the_transcript_not_just_the_last_line(
    ) {
        let stdout = concat!(
            r#"{"msg":{"type":"agent_message","message":"working"}}"#,
            "\n",
            r#"{"msg":{"type":"token_count","input_tokens":7,"output_tokens":2}}"#,
            "\n",
            r#"{"msg":{"type":"task_complete","last_agent_message":""}}"#,
            "\n",
        );
        let usage = CodexAdapter.extract_usage(stdout).unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 2);
    }

    #[test]
    fn codex_extract_usage_keeps_the_last_token_count_event_when_several_are_reported() {
        let stdout = concat!(
            r#"{"msg":{"type":"token_count","input_tokens":7,"output_tokens":2}}"#,
            "\n",
            r#"{"msg":{"type":"token_count","input_tokens":20,"output_tokens":9}}"#,
            "\n",
        );
        let usage = CodexAdapter.extract_usage(stdout).unwrap();
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 9);
    }

    #[test]
    fn codex_extract_usage_returns_none_when_no_token_count_event_is_present() {
        let stdout = r#"{"msg":{"type":"task_complete","last_agent_message":"done"}}"#;
        assert_eq!(CodexAdapter.extract_usage(stdout), None);
    }

    #[test]
    fn codex_extract_usage_returns_none_for_output_that_is_not_the_event_envelope() {
        assert_eq!(CodexAdapter.extract_usage("not json at all"), None);
    }

    #[test]
    fn codex_extract_usage_returns_none_for_completely_empty_output() {
        assert_eq!(CodexAdapter.extract_usage(""), None);
    }

    #[test]
    fn every_role_has_a_non_blank_codex_default_prompt() {
        for role in [AgentRole::Coder, AgentRole::Reviewer, AgentRole::Tester] {
            assert!(!CodexAdapter.default_prompt(role).trim().is_empty());
        }
    }

    #[test]
    fn every_role_has_a_non_blank_codex_default_tools_grant() {
        for role in [AgentRole::Coder, AgentRole::Reviewer, AgentRole::Tester] {
            let tools = CodexAdapter
                .default_tools(role)
                .expect("every role must have a default sandbox grant");
            assert!(!tools.trim().is_empty());
        }
    }

    #[test]
    fn the_codex_reviewer_default_sandbox_is_read_only() {
        assert_eq!(
            CodexAdapter.default_tools(AgentRole::Reviewer).unwrap(),
            "read-only"
        );
    }

    #[test]
    fn the_codex_coder_and_tester_default_sandbox_is_workspace_write() {
        for role in [AgentRole::Coder, AgentRole::Tester] {
            assert_eq!(
                CodexAdapter.default_tools(role).unwrap(),
                "workspace-write",
                "{role:?}"
            );
        }
    }

    // ===================================================================
    // `MistralAdapter` (issue #71) -- no structured output format is
    // assumed at all (see `MistralAdapter`'s own docs); these fixtures
    // exercise the "whole trimmed stdout is the final answer" contract that
    // choice implies, independent of any particular CLI wire format.
    // ===================================================================

    #[test]
    fn mistral_build_command_passes_the_system_prompt_via_the_system_flag() {
        let command = MistralAdapter
            .build_command(&definition(None, None))
            .unwrap();
        assert_eq!(command.program, "mistral");
        assert_eq!(command.args, vec!["--system", "be an agent"]);
    }

    #[test]
    fn mistral_build_command_appends_model_when_the_definition_sets_one() {
        let command = MistralAdapter
            .build_command(&definition(Some("mistral-large"), None))
            .unwrap();
        assert!(command
            .args
            .windows(2)
            .any(|w| w == ["--model", "mistral-large"]));
    }

    #[test]
    fn mistral_build_command_ignores_a_tools_grant_the_definition_sets() {
        // No known equivalent of `--allowedTools`/`--sandbox` for this CLI
        // (see `MistralAdapter`'s own docs) -- `tools` must not leak into
        // argv as some unrecognized flag.
        let command = MistralAdapter
            .build_command(&definition(None, Some("Read, Write, Edit, Bash")))
            .unwrap();
        assert_eq!(command.args, vec!["--system", "be an agent"]);
    }

    #[test]
    fn mistral_env_allowlist_is_exactly_home() {
        assert_eq!(MistralAdapter.env_allowlist(), &["HOME"]);
    }

    #[test]
    fn mistral_extract_findings_treats_the_whole_trimmed_stdout_as_ndjson_findings() {
        let stdout =
            "{\"source\":\"tester\",\"severity\":\"warning\",\"description\":\"flaky test\"}\n";
        let findings = MistralAdapter.extract_findings(stdout).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].description, "flaky test");
    }

    #[test]
    fn mistral_extract_findings_treats_blank_only_output_as_no_findings_error() {
        let error = MistralAdapter.extract_findings("   \n\n").unwrap_err();
        assert!(matches!(
            error,
            warden_core::CoreError::MalformedAgentOutput(_)
        ));
    }

    #[test]
    fn mistral_extract_findings_rejects_completely_empty_output() {
        let error = MistralAdapter.extract_findings("").unwrap_err();
        assert!(matches!(
            error,
            warden_core::CoreError::MalformedAgentOutput(_)
        ));
    }

    #[test]
    fn mistral_extract_findings_propagates_the_parse_error_for_malformed_findings() {
        assert!(MistralAdapter.extract_findings("not ndjson").is_err());
    }

    #[test]
    fn mistral_extract_usage_always_returns_none() {
        // No structured usage-reporting format is known for this CLI (see
        // `MistralAdapter`'s own docs) -- "n/a" (`None`) regardless of
        // input, never a fabricated zero.
        assert_eq!(MistralAdapter.extract_usage("anything at all"), None);
        assert_eq!(MistralAdapter.extract_usage(""), None);
    }

    #[test]
    fn mistral_parse_progress_line_uses_the_trait_default_of_none() {
        // No override (see `MistralAdapter`'s own docs): every line
        // degrades to the pre-issue-#33 silence, the trait's own
        // legitimate default.
        assert_eq!(MistralAdapter.parse_progress_line("anything at all"), None);
    }

    #[test]
    fn every_role_has_a_non_blank_mistral_default_prompt() {
        for role in [AgentRole::Coder, AgentRole::Reviewer, AgentRole::Tester] {
            assert!(!MistralAdapter.default_prompt(role).trim().is_empty());
        }
    }

    #[test]
    fn every_role_has_no_mistral_default_tools_grant() {
        // Legitimate `None` (see `MistralAdapter::default_tools`'s own
        // docs), not an oversight -- pinned explicitly so a future change
        // that starts returning `Some` here is a deliberate decision, not a
        // silent one.
        for role in [AgentRole::Coder, AgentRole::Reviewer, AgentRole::Tester] {
            assert_eq!(MistralAdapter.default_tools(role), None, "{role:?}");
        }
    }
}
