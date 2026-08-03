use warden_core::{AgentDefinition, Finding, RateLimitStatus, TokenUsage};

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
}
