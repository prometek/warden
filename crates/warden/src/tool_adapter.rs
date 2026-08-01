use warden_core::{AgentDefinition, AgentRole, Finding, RateLimitStatus, TokenUsage};

use crate::error::Result;
use crate::process::AgentCommand;

/// Stable identity of a built-in agent CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolName {
    Claude,
    Codex,
    Mistral,
}

impl ToolName {
    pub fn parse(raw: &str) -> std::result::Result<Self, String> {
        match raw {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "mistral" => Ok(Self::Mistral),
            other => Err(format!(
                "unknown tool {other:?} (supported: \"claude\", \"codex\", \"mistral\")"
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Mistral => "mistral",
        }
    }
}

/// Turns a role's definition into the command to spawn for it, plus everything else specific to one
/// tool CLI.
pub trait ToolAdapter: Sync {
    /// Builds the concrete CLI invocation for `definition`.
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand>;

    /// Environment variable names (beyond `PATH`, which `warden_sandbox::LocalSandbox::execute`
    /// always forwards) this tool needs to find its own configuration/auth.
    fn env_allowlist(&self) -> &'static [&'static str];

    fn extract_findings(&self, stdout: &str) -> warden_core::Result<Vec<Finding>>;

    /// The system prompt a role runs with when the base repo has no `.warden/agents/<role>.md`:
    /// what lets `warden run --tool claude` work with zero markdown at all.
    fn default_prompt(&self, role: AgentRole) -> &'static str;

    fn default_tools(&self, role: AgentRole) -> Option<&'static str>;

    fn parse_progress_line(&self, _line: &str) -> Option<String> {
        None
    }

    fn extract_usage(&self, _stdout: &str) -> Option<TokenUsage> {
        None
    }

    fn extract_rate_limit(&self, _stdout: &str) -> Option<RateLimitStatus> {
        None
    }
}

impl ToolAdapter for ToolName {
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand> {
        match self {
            Self::Claude => ClaudeAdapter.build_command(definition),
            Self::Codex => CodexAdapter.build_command(definition),
            Self::Mistral => MistralAdapter.build_command(definition),
        }
    }

    fn env_allowlist(&self) -> &'static [&'static str] {
        match self {
            Self::Claude => ClaudeAdapter.env_allowlist(),
            Self::Codex => CodexAdapter.env_allowlist(),
            Self::Mistral => MistralAdapter.env_allowlist(),
        }
    }

    fn extract_findings(&self, stdout: &str) -> warden_core::Result<Vec<Finding>> {
        match self {
            Self::Claude => ClaudeAdapter.extract_findings(stdout),
            Self::Codex => CodexAdapter.extract_findings(stdout),
            Self::Mistral => MistralAdapter.extract_findings(stdout),
        }
    }

    fn default_prompt(&self, role: AgentRole) -> &'static str {
        match self {
            Self::Claude => ClaudeAdapter.default_prompt(role),
            Self::Codex => CodexAdapter.default_prompt(role),
            Self::Mistral => MistralAdapter.default_prompt(role),
        }
    }

    fn default_tools(&self, role: AgentRole) -> Option<&'static str> {
        match self {
            Self::Claude => ClaudeAdapter.default_tools(role),
            Self::Codex => CodexAdapter.default_tools(role),
            Self::Mistral => MistralAdapter.default_tools(role),
        }
    }

    fn parse_progress_line(&self, line: &str) -> Option<String> {
        match self {
            Self::Claude => ClaudeAdapter.parse_progress_line(line),
            Self::Codex => CodexAdapter.parse_progress_line(line),
            Self::Mistral => MistralAdapter.parse_progress_line(line),
        }
    }

    fn extract_usage(&self, stdout: &str) -> Option<TokenUsage> {
        match self {
            Self::Claude => ClaudeAdapter.extract_usage(stdout),
            Self::Codex => CodexAdapter.extract_usage(stdout),
            Self::Mistral => MistralAdapter.extract_usage(stdout),
        }
    }

    fn extract_rate_limit(&self, stdout: &str) -> Option<RateLimitStatus> {
        match self {
            Self::Claude => ClaudeAdapter.extract_rate_limit(stdout),
            Self::Codex => CodexAdapter.extract_rate_limit(stdout),
            Self::Mistral => MistralAdapter.extract_rate_limit(stdout),
        }
    }
}

/// The `claude` adapter: Warden's first built-in [`ToolAdapter`], wrapping the `claude` CLI (Claude
/// Code) in non-interactive print mode.
pub struct ClaudeAdapter;

#[derive(Debug, serde::Deserialize)]
struct ClaudeResultEnvelope {
    result: Option<String>,
    #[serde(default)]
    usage: Option<ClaudeUsage>,
}

#[derive(Debug, serde::Deserialize)]
struct ClaudeUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

/// Stage one of `extract_rate_limit`'s two-stage parse: decodes *only* the `type` discriminant
/// every `--output-format stream-json` line carries.
#[derive(Debug, serde::Deserialize)]
struct ClaudeLineKind {
    #[serde(rename = "type")]
    kind: String,
}

/// Stage two of `extract_rate_limit`'s two-stage parse -- only attempted once [`ClaudeLineKind`]
/// has already confirmed a line's `type` is `rate_limit_event`.
#[derive(Debug, serde::Deserialize)]
struct ClaudeRateLimitEnvelope {
    rate_limit_info: ClaudeRateLimitInfo,
}

/// `claude`'s own `rate_limit_info` object, nested in [`ClaudeRateLimitEnvelope`] -- verified
/// directly against the real CLI.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeRateLimitInfo {
    status: warden_core::RateLimitState,
    resets_at: i64,
    rate_limit_type: warden_core::RateLimitWindow,
    /// Fraction in `0.0..=1.0`, **not** a percentage -- observed `0.93` for 93% utilization.
    utilization: f64,
    is_using_overage: bool,
    surpassed_threshold: f64,
}

const MAX_PLAUSIBLE_QUOTA_FRACTION: f64 = 2.0;

/// Boundary validation for [`ClaudeRateLimitInfo`]'s numeric fields.
fn validate_rate_limit_info(info: &ClaudeRateLimitInfo) -> std::result::Result<(), String> {
    if !info.utilization.is_finite()
        || info.utilization < 0.0
        || info.utilization > MAX_PLAUSIBLE_QUOTA_FRACTION
    {
        return Err(format!(
            "utilization {} is outside the plausible 0.0..={MAX_PLAUSIBLE_QUOTA_FRACTION} \
             fraction range",
            info.utilization
        ));
    }
    if !info.surpassed_threshold.is_finite()
        || info.surpassed_threshold < 0.0
        || info.surpassed_threshold > MAX_PLAUSIBLE_QUOTA_FRACTION
    {
        return Err(format!(
            "surpassed_threshold {} is outside the plausible 0.0..={MAX_PLAUSIBLE_QUOTA_FRACTION} \
             fraction range",
            info.surpassed_threshold
        ));
    }
    if info.resets_at <= 0 {
        return Err(format!(
            "resets_at {} is not a plausible positive unix timestamp",
            info.resets_at
        ));
    }
    Ok(())
}

const CLAUDE_ENV_ALLOWLIST: &[&str] = &["HOME", "USER"];

impl ToolAdapter for ClaudeAdapter {
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand> {
        let mut args = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
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
            AgentRole::Coder => "Read, Write, Edit, Bash",
            AgentRole::Reviewer => "Read, Grep, Glob, Bash",
            AgentRole::Tester => "Read, Write, Edit, Grep, Glob, Bash",
        })
    }

    fn parse_progress_line(&self, line: &str) -> Option<String> {
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

    fn extract_rate_limit(&self, stdout: &str) -> Option<RateLimitStatus> {
        for line in stdout.lines().rev() {
            let Ok(kind) = serde_json::from_str::<ClaudeLineKind>(line) else {
                continue;
            };
            if kind.kind != "rate_limit_event" {
                continue;
            }
            // The newest `rate_limit_event` in the scanned range -- from here on, always return
            // (`Some` or `None`), never `continue`.
            let envelope = match serde_json::from_str::<ClaudeRateLimitEnvelope>(line) {
                Ok(envelope) => envelope,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        line,
                        "claude rate_limit_event line's rate_limit_info failed to decode -- \
                         treating this invocation's rate-limit status as n/a rather than \
                         falling back to an older, stale report"
                    );
                    return None;
                }
            };
            let info = envelope.rate_limit_info;
            if let Err(reason) = validate_rate_limit_info(&info) {
                tracing::warn!(
                    reason,
                    line,
                    "claude rate_limit_event line failed numeric range validation -- \
                     treating this invocation's rate-limit status as n/a rather than \
                     falling back to an older, stale report"
                );
                return None;
            }
            return Some(RateLimitStatus::new(
                info.status,
                info.rate_limit_type,
                info.utilization,
                info.is_using_overage,
                info.surpassed_threshold,
                info.resets_at,
            ));
        }
        None
    }
}

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

/// How much of a progress line's text is worth showing a live observer.
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

/// Field names checked, in order, for a short human-readable summary of a `tool_use` block's
/// `input`.
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

/// The `codex` adapter: Warden's second built-in [`ToolAdapter`], wrapping the OpenAI Codex CLI
/// (the `codex` binary) in its non-interactive `codex exec` mode.
pub struct CodexAdapter;

/// Env vars `codex` needs beyond `PATH` to find its own configuration and credentials.
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
        args.push("--".to_string());
        args.push(definition.system_prompt.clone());
        Ok(AgentCommand::new("codex", args))
    }

    fn env_allowlist(&self) -> &'static [&'static str] {
        CODEX_ENV_ALLOWLIST
    }

    fn extract_findings(&self, stdout: &str) -> warden_core::Result<Vec<Finding>> {
        enum Terminal {
            Complete(Option<String>),
            Failed(String),
        }

        if stdout.trim().is_empty() {
            return Err(warden_core::CoreError::MalformedAgentOutput(
                "codex produced no output at all (expected at least a final `task_complete` \
                 event)"
                    .to_string(),
            ));
        }

        let terminal = stdout.lines().rev().find_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let event: CodexEvent = serde_json::from_str(line).ok()?;
            match event.msg {
                CodexEventMsg::TaskComplete { last_agent_message } => {
                    Some(Terminal::Complete(last_agent_message))
                }
                CodexEventMsg::Error { message } => Some(Terminal::Failed(message)),
                CodexEventMsg::AgentMessage { .. }
                | CodexEventMsg::TokenCount { .. }
                | CodexEventMsg::Other => None,
            }
        });

        match terminal {
            Some(Terminal::Complete(Some(text))) => warden_core::parse_findings(&text),
            Some(Terminal::Complete(None)) => Err(warden_core::CoreError::MalformedAgentOutput(
                "codex reported task_complete with no last_agent_message (the agent likely did \
                 not complete normally)"
                    .to_string(),
            )),
            Some(Terminal::Failed(message)) => Err(warden_core::CoreError::MalformedAgentOutput(
                format!("codex reported an error: {message}"),
            )),
            None => Err(warden_core::CoreError::MalformedAgentOutput(
                "codex's output contains no `task_complete`/`error` event".to_string(),
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
            AgentRole::Coder => "workspace-write",
            AgentRole::Reviewer => "read-only",
            AgentRole::Tester => "workspace-write",
        })
    }

    fn parse_progress_line(&self, line: &str) -> Option<String> {
        let event: CodexEvent = serde_json::from_str(line).ok()?;
        match event.msg {
            CodexEventMsg::AgentMessage { message } => {
                Some(format!("message: {}", summarize_progress_text(&message)))
            }
            _ => None,
        }
    }

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

#[derive(Debug, serde::Deserialize)]
struct CodexEvent {
    msg: CodexEventMsg,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexEventMsg {
    AgentMessage {
        message: String,
    },
    /// The stream's terminal event -- `extract_findings`'s target, the `codex` analogue of
    /// `claude`'s own `result` envelope.
    TaskComplete {
        #[serde(default)]
        last_agent_message: Option<String>,
    },
    /// 's source for this adapter -- required, non-`Option` fields.
    TokenCount {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// A terminal failure this invocation reported on its own -- distinct from a malformed line
    /// `extract_findings` itself can't parse at all.
    Error {
        message: String,
    },
    #[serde(other)]
    Other,
}

/// The `mistral` adapter: Warden's third built-in [`ToolAdapter`], wrapping a `mistral` CLI in the
/// most conservative shape defensible without a live install to verify against.
pub struct MistralAdapter;

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
        warden_core::parse_findings(stdout.trim())
    }

    fn default_prompt(&self, role: AgentRole) -> &'static str {
        match role {
            AgentRole::Coder => DEFAULT_CODER_PROMPT,
            AgentRole::Reviewer => DEFAULT_REVIEWER_PROMPT,
            AgentRole::Tester => DEFAULT_TESTER_PROMPT,
        }
    }

    fn default_tools(&self, _role: AgentRole) -> Option<&'static str> {
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

    #[test]
    fn parse_progress_line_skips_unrecognized_content_block_types_without_failing() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"pondering..."}]}}"#;
        assert_eq!(ClaudeAdapter.parse_progress_line(line), None);
    }

    #[test]
    fn parse_progress_line_handles_a_real_captured_tool_use_line() {
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg_011Cd9HiRpTrNZbpg6SsxFb7","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_01GLvBUCpwy33TmUv2zhwwWc","name":"Bash","input":{"command":"ls -la /private/tmp/stream-json-probe","description":"List files in working dir"},"caller":{"type":"direct"}}],"stop_reason":null,"stop_sequence":null,"stop_details":null,"usage":{"input_tokens":2},"diagnostics":null,"context_management":null},"parent_tool_use_id":null,"session_id":"4d83aff1-794c-4154-8cbf-34a6beb3423a","uuid":"dd65275f-4dc6-402b-bc2b-d7c5bf62f8af","timestamp":"2026-07-18T09:30:28.145Z","request_id":"req_011Cd9HiKv1bhZVTuY3FD96a"}"#;
        let progress = ClaudeAdapter.parse_progress_line(line).unwrap();
        assert_eq!(
            progress,
            "tool_use: Bash (ls -la /private/tmp/stream-json-probe)"
        );
    }

    #[test]
    fn extract_findings_handles_a_real_captured_result_line() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"api_error_status":null,"duration_ms":8120,"result":"Files:\n- `err.log` — empty\n- `out.ndjson` — 11.7 KB\n\nHello.","stop_reason":"end_turn","session_id":"4d83aff1-794c-4154-8cbf-34a6beb3423a","total_cost_usd":0.1483495,"permission_denials":[],"terminal_reason":"completed","uuid":"1d4ae256-0ea2-4dc3-addd-526a68c08806"}"#;
        let error = match ClaudeAdapter.extract_findings(stdout).unwrap_err() {
            warden_core::CoreError::MalformedAgentOutput(message) => message,
            other => panic!("expected MalformedAgentOutput, got {other:?}"),
        };
        assert!(
            !error.contains("envelope"),
            "must fail on the inner NDJSON, not on unwrapping the envelope: {error}"
        );
    }

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

    #[test]
    fn extract_usage_returns_none_for_the_real_captured_result_line_with_no_usage_field() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"api_error_status":null,"duration_ms":8120,"result":"Files:\n- `err.log` — empty\n- `out.ndjson` — 11.7 KB\n\nHello.","stop_reason":"end_turn","session_id":"4d83aff1-794c-4154-8cbf-34a6beb3423a","total_cost_usd":0.1483495,"permission_denials":[],"terminal_reason":"completed","uuid":"1d4ae256-0ea2-4dc3-addd-526a68c08806"}"#;
        assert_eq!(ClaudeAdapter.extract_usage(stdout), None);
    }

    const REAL_CAPTURED_RATE_LIMIT_EVENT_LINE: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":0.93,"isUsingOverage":false,"surpassedThreshold":0.75},"uuid":"21c05092-e021-402f-bee8-df86ed81af44","session_id":"cc97c92a-3093-421b-a6f1-ecb2b3546855"}"#;

    #[test]
    fn extract_rate_limit_reads_every_field_from_the_real_captured_event_line() {
        let status = ClaudeAdapter
            .extract_rate_limit(REAL_CAPTURED_RATE_LIMIT_EVENT_LINE)
            .unwrap();
        assert_eq!(status.status, warden_core::RateLimitState::AllowedWarning);
        assert_eq!(
            status.rate_limit_type,
            warden_core::RateLimitWindow::SevenDay
        );
        assert_eq!(status.utilization, 0.93);
        assert!(!status.is_using_overage);
        assert_eq!(status.surpassed_threshold, 0.75);
        assert_eq!(status.resets_at, 1785686400);
    }

    #[test]
    fn extract_rate_limit_and_parse_progress_line_both_see_the_same_real_line_differently() {
        assert_eq!(
            ClaudeAdapter.parse_progress_line(REAL_CAPTURED_RATE_LIMIT_EVENT_LINE),
            None
        );
        assert!(ClaudeAdapter
            .extract_rate_limit(REAL_CAPTURED_RATE_LIMIT_EVENT_LINE)
            .is_some());
    }

    #[test]
    fn extract_rate_limit_returns_none_when_no_rate_limit_event_is_present() {
        let stdout = concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#,
        );
        assert_eq!(ClaudeAdapter.extract_rate_limit(stdout), None);
    }

    #[test]
    fn extract_rate_limit_returns_none_for_output_that_is_not_json_at_all() {
        assert_eq!(ClaudeAdapter.extract_rate_limit("not json at all"), None);
    }

    #[test]
    fn extract_rate_limit_returns_none_for_completely_empty_output() {
        assert_eq!(ClaudeAdapter.extract_rate_limit(""), None);
    }

    #[test]
    fn extract_rate_limit_keeps_the_last_event_when_several_are_reported_across_turns() {
        let stdout = concat!(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":0.93,"isUsingOverage":false,"surpassedThreshold":0.75}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"still working"}]}}"#,
            "\n",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":0.94,"isUsingOverage":false,"surpassedThreshold":0.75}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#,
        );
        let status = ClaudeAdapter.extract_rate_limit(stdout).unwrap();
        assert_eq!(status.utilization, 0.94);
    }

    #[test]
    fn extract_rate_limit_tolerates_an_unrecognized_status_and_rate_limit_type() {
        let stdout = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"blocked","resetsAt":1785686400,"rateLimitType":"five_hour","utilization":1.0,"isUsingOverage":true,"surpassedThreshold":0.97}}"#;
        let status = ClaudeAdapter.extract_rate_limit(stdout).unwrap();
        assert_eq!(
            status.status,
            warden_core::RateLimitState::Other("blocked".to_string())
        );
        assert_eq!(
            status.rate_limit_type,
            warden_core::RateLimitWindow::Other("five_hour".to_string())
        );
    }

    #[test]
    fn extract_rate_limit_accepts_a_legitimate_overage_utilization_past_1_0() {
        let stdout = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":1.05,"isUsingOverage":true,"surpassedThreshold":0.97}}"#;
        let status = ClaudeAdapter.extract_rate_limit(stdout).unwrap();
        assert_eq!(status.utilization, 1.05);
        assert!(status.is_using_overage);
    }

    #[test]
    fn extract_rate_limit_rejects_a_utilization_that_looks_like_a_percentage() {
        let stdout = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":93.0,"isUsingOverage":false,"surpassedThreshold":0.75}}"#;
        assert_eq!(ClaudeAdapter.extract_rate_limit(stdout), None);
    }

    #[test]
    fn extract_rate_limit_rejects_a_negative_utilization() {
        let stdout = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":-0.1,"isUsingOverage":false,"surpassedThreshold":0.75}}"#;
        assert_eq!(ClaudeAdapter.extract_rate_limit(stdout), None);
    }

    #[test]
    fn extract_rate_limit_rejects_a_utilization_that_is_not_a_number_at_all() {
        let stdout = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":null,"isUsingOverage":false,"surpassedThreshold":0.75}}"#;
        assert_eq!(ClaudeAdapter.extract_rate_limit(stdout), None);
    }

    #[test]
    fn extract_rate_limit_rejects_an_out_of_range_surpassed_threshold() {
        let stdout = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":0.93,"isUsingOverage":false,"surpassedThreshold":75.0}}"#;
        assert_eq!(ClaudeAdapter.extract_rate_limit(stdout), None);
    }

    #[test]
    fn extract_rate_limit_rejects_a_non_positive_resets_at() {
        let stdout = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":0,"rateLimitType":"seven_day","utilization":0.93,"isUsingOverage":false,"surpassedThreshold":0.75}}"#;
        assert_eq!(ClaudeAdapter.extract_rate_limit(stdout), None);

        let stdout_negative = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":-100,"rateLimitType":"seven_day","utilization":0.93,"isUsingOverage":false,"surpassedThreshold":0.75}}"#;
        assert_eq!(ClaudeAdapter.extract_rate_limit(stdout_negative), None);
    }

    #[test]
    fn a_malformed_newest_rate_limit_event_does_not_fall_back_to_an_older_valid_one() {
        let stdout_undecodable = concat!(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":0.93,"isUsingOverage":false,"surpassedThreshold":0.75}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"still working"}]}}"#,
            "\n",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning"}}"#,
        );
        assert_eq!(
            ClaudeAdapter.extract_rate_limit(stdout_undecodable),
            None,
            "an undecodable newest rate_limit_event must yield n/a, not the older 0.93 report"
        );

        let stdout_out_of_range = concat!(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":0.93,"isUsingOverage":false,"surpassedThreshold":0.75}}"#,
            "\n",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":93.0,"isUsingOverage":false,"surpassedThreshold":0.75}}"#,
        );
        assert_eq!(
            ClaudeAdapter.extract_rate_limit(stdout_out_of_range),
            None,
            "an out-of-range newest rate_limit_event must yield n/a, not the older 0.93 report"
        );
    }

    fn rate_limit_line_with_utilization(utilization: &str) -> String {
        format!(
            r#"{{"type":"rate_limit_event","rate_limit_info":{{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":{utilization},"isUsingOverage":true,"surpassedThreshold":0.97}}}}"#
        )
    }

    #[test]
    fn extract_rate_limit_accepts_up_to_the_plausible_ceiling_and_rejects_past_it() {
        for accepted in ["0.0", "1.0", "1.9999", "2.0"] {
            let stdout = rate_limit_line_with_utilization(accepted);
            assert!(
                ClaudeAdapter.extract_rate_limit(&stdout).is_some(),
                "utilization {accepted} is within the plausible range and must be accepted"
            );
        }
        for rejected in ["2.0001", "3.0", "93.0"] {
            let stdout = rate_limit_line_with_utilization(rejected);
            assert_eq!(
                ClaudeAdapter.extract_rate_limit(&stdout),
                None,
                "utilization {rejected} is past the plausible ceiling and must be rejected"
            );
        }
    }

    #[test]
    fn extract_rate_limit_treats_negative_zero_utilization_as_zero() {
        let stdout = rate_limit_line_with_utilization("-0.0");
        let status = ClaudeAdapter.extract_rate_limit(&stdout).unwrap();
        assert_eq!(status.utilization, 0.0);
    }

    #[test]
    fn extract_rate_limit_accepts_a_far_future_resets_at_and_rejects_the_extreme_negative() {
        let far_future = format!(
            r#"{{"type":"rate_limit_event","rate_limit_info":{{"status":"allowed_warning","resetsAt":{},"rateLimitType":"seven_day","utilization":0.93,"isUsingOverage":false,"surpassedThreshold":0.75}}}}"#,
            i64::MAX
        );
        assert_eq!(
            ClaudeAdapter
                .extract_rate_limit(&far_future)
                .unwrap()
                .resets_at,
            i64::MAX
        );

        let extreme_negative = format!(
            r#"{{"type":"rate_limit_event","rate_limit_info":{{"status":"allowed_warning","resetsAt":{},"rateLimitType":"seven_day","utilization":0.93,"isUsingOverage":false,"surpassedThreshold":0.75}}}}"#,
            i64::MIN
        );
        assert_eq!(ClaudeAdapter.extract_rate_limit(&extreme_negative), None);
    }

    #[test]
    fn extract_rate_limit_reads_the_event_through_crlf_line_endings() {
        let stdout = format!(
            "{}\r\n{}\r\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}"#,
            REAL_CAPTURED_RATE_LIMIT_EVENT_LINE
        );
        let status = ClaudeAdapter.extract_rate_limit(&stdout).unwrap();
        assert_eq!(status.utilization, 0.93);
    }

    #[test]
    fn extract_rate_limit_is_unaffected_by_a_trailing_newline() {
        let with_newline = format!("{REAL_CAPTURED_RATE_LIMIT_EVENT_LINE}\n");
        assert_eq!(
            ClaudeAdapter.extract_rate_limit(&with_newline),
            ClaudeAdapter.extract_rate_limit(REAL_CAPTURED_RATE_LIMIT_EVENT_LINE)
        );
    }

    #[test]
    fn extract_rate_limit_returns_none_when_the_only_event_present_is_malformed() {
        let stdout = concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}"#,
            "\n",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning"}}"#,
        );
        assert_eq!(ClaudeAdapter.extract_rate_limit(stdout), None);
    }

    #[test]
    fn every_role_has_a_non_blank_default_prompt() {
        for role in [AgentRole::Coder, AgentRole::Reviewer, AgentRole::Tester] {
            assert!(!ClaudeAdapter.default_prompt(role).trim().is_empty());
        }
    }

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
                "--",
                "be an agent"
            ]
        );
    }

    #[test]
    fn codex_build_command_places_the_end_of_options_separator_right_before_the_prompt() {
        let command = CodexAdapter.build_command(&definition(None, None)).unwrap();
        let prompt_index = command.args.len() - 1;
        assert_eq!(command.args[prompt_index], "be an agent");
        assert_eq!(command.args[prompt_index - 1], "--");
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
    fn codex_extract_rate_limit_uses_the_trait_default_of_none() {
        assert_eq!(CodexAdapter.extract_rate_limit("anything at all"), None);
        assert_eq!(CodexAdapter.extract_rate_limit(""), None);
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
    fn mistral_extract_findings_treats_blank_only_output_as_no_findings() {
        assert_eq!(
            MistralAdapter.extract_findings("   \n\n").unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn mistral_extract_findings_treats_completely_empty_output_as_no_findings() {
        assert_eq!(MistralAdapter.extract_findings("").unwrap(), Vec::new());
    }

    #[test]
    fn mistral_extract_findings_propagates_the_parse_error_for_malformed_findings() {
        assert!(MistralAdapter.extract_findings("not ndjson").is_err());
    }

    #[test]
    fn mistral_extract_usage_always_returns_none() {
        assert_eq!(MistralAdapter.extract_usage("anything at all"), None);
        assert_eq!(MistralAdapter.extract_usage(""), None);
    }

    #[test]
    fn mistral_parse_progress_line_uses_the_trait_default_of_none() {
        assert_eq!(MistralAdapter.parse_progress_line("anything at all"), None);
    }

    #[test]
    fn mistral_extract_rate_limit_uses_the_trait_default_of_none() {
        assert_eq!(MistralAdapter.extract_rate_limit("anything at all"), None);
        assert_eq!(MistralAdapter.extract_rate_limit(""), None);
    }

    #[test]
    fn every_role_has_a_non_blank_mistral_default_prompt() {
        for role in [AgentRole::Coder, AgentRole::Reviewer, AgentRole::Tester] {
            assert!(!MistralAdapter.default_prompt(role).trim().is_empty());
        }
    }

    #[test]
    fn every_role_has_no_mistral_default_tools_grant() {
        for role in [AgentRole::Coder, AgentRole::Reviewer, AgentRole::Tester] {
            assert_eq!(MistralAdapter.default_tools(role), None, "{role:?}");
        }
    }
}
