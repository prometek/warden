use serde::Deserialize;

use crate::error::{CoreError, Result};

const FRONTMATTER_FENCE: &str = "---";

/// The frontmatter's keys -- exactly Claude Code's own subagent schema
/// (`name`/`description`/`tools`/`model`), nothing warden-native added on top.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontmatterWire {
    name: Option<String>,
    description: Option<String>,
    tools: Option<String>,
    /// A model alias (`"sonnet"`, `"opus"`,...) or full model name, passed through verbatim to
    /// `--model` by the adapter.
    model: Option<String>,
}

/// A parsed, validated markdown agent definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    /// Accepted for Claude Code file compatibility; Warden has no operational use for it.
    pub name: Option<String>,
    /// Accepted for Claude Code file compatibility; Warden has no operational use for it.
    pub description: Option<String>,
    /// Passed to `--allowedTools`/equivalent by the tool adapter, verbatim.
    pub tools: Option<String>,
    /// Passed to `--model`/equivalent by the tool adapter, verbatim.
    pub model: Option<String>,
    /// The markdown body after the frontmatter: what this role *is*.
    pub system_prompt: String,
}

impl AgentDefinition {
    pub fn new(
        name: Option<String>,
        description: Option<String>,
        tools: Option<String>,
        model: Option<String>,
        system_prompt: impl Into<String>,
    ) -> Result<Self> {
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
            name: reject_blank_if_present("name", name)?,
            description: reject_blank_if_present("description", description)?,
            tools: reject_blank_if_present("tools", tools)?,
            model: reject_blank_if_present("model", model)?,
            system_prompt: trimmed.to_string(),
        })
    }
}

fn reject_blank_if_present(field: &'static str, value: Option<String>) -> Result<Option<String>> {
    match value {
        Some(raw) if raw.trim().is_empty() => Err(CoreError::MalformedAgentDefinition(format!(
            "agent definition `{field}` must not be blank when present (omit the key entirely \
             to leave it unset)"
        ))),
        other => Ok(other),
    }
}

pub fn parse_agent_definition(raw: &str) -> Result<AgentDefinition> {
    let (frontmatter, body) = split_frontmatter(raw)?;
    let wire = parse_frontmatter(frontmatter)?;
    AgentDefinition::new(wire.name, wire.description, wire.tools, wire.model, body)
}

/// A UTF-8 BOM: legal in a text file, and invisible in an editor, but it sits *before* the opening
/// fence and would make the fence check fail while the first line visibly reads `---`.
const BYTE_ORDER_MARK: &str = "\u{feff}";

/// Splits `---\n<frontmatter>\n---\n<body>` into its two halves.
fn split_frontmatter(raw: &str) -> Result<(&str, &str)> {
    // A CRLF or BOM file would otherwise fail the fence check below with "must start with a `---`
    // fence on its own first line" -- about a file whose first line *is* visibly `---`.
    if let Some(rest) = raw.strip_prefix(BYTE_ORDER_MARK) {
        let hint = if rest.starts_with(FRONTMATTER_FENCE) {
            " (the `---` fence itself looks fine -- the BOM is what precedes it)"
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
    if let Some(body) = rest.strip_prefix(&format!("{FRONTMATTER_FENCE}\n")) {
        return Ok(("", body));
    }
    if rest == FRONTMATTER_FENCE {
        return Ok(("", ""));
    }
    if let Some(frontmatter) = rest.strip_suffix(&format!("\n{FRONTMATTER_FENCE}")) {
        return Ok((frontmatter, ""));
    }
    Err(CoreError::MalformedAgentDefinition(format!(
        "agent definition frontmatter is never closed by a `{FRONTMATTER_FENCE}` fence on its \
         own line"
    )))
}

/// Deserializes the frontmatter block into [`FrontmatterWire`].
fn parse_frontmatter(frontmatter: &str) -> Result<FrontmatterWire> {
    if frontmatter.trim().is_empty() {
        return Ok(FrontmatterWire::default());
    }
    serde_yaml::from_str(frontmatter).map_err(|error| {
        CoreError::MalformedAgentDefinition(format!("invalid frontmatter: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_DEFINITION: &str = "---\n\
        name: coder\n\
        description: Implements the task on the working branch.\n\
        tools: Read, Edit, Bash\n\
        model: sonnet\n\
        ---\n\
        \n\
        You are Warden's coder.\n\
        \n\
        Read the JSON payload on stdin.\n";

    #[test]
    fn parses_a_full_definition_with_every_frontmatter_key_and_the_system_prompt() {
        let definition = parse_agent_definition(FULL_DEFINITION).unwrap();
        assert_eq!(definition.name.as_deref(), Some("coder"));
        assert_eq!(
            definition.description.as_deref(),
            Some("Implements the task on the working branch.")
        );
        assert_eq!(definition.tools.as_deref(), Some("Read, Edit, Bash"));
        assert_eq!(definition.model.as_deref(), Some("sonnet"));
        assert_eq!(
            definition.system_prompt,
            "You are Warden's coder.\n\nRead the JSON payload on stdin."
        );
    }

    #[test]
    fn every_frontmatter_key_is_optional() {
        let raw = "---\n---\nreview it\n";
        let definition = parse_agent_definition(raw).unwrap();
        assert_eq!(definition.name, None);
        assert_eq!(definition.description, None);
        assert_eq!(definition.tools, None);
        assert_eq!(definition.model, None);
        assert_eq!(definition.system_prompt, "review it");
    }

    #[test]
    fn a_blank_but_present_frontmatter_block_is_treated_as_no_keys_at_all() {
        let raw = "---\n   \n\t\n---\nreview it\n";
        let definition = parse_agent_definition(raw).unwrap();
        assert_eq!(definition.name, None);
        assert_eq!(definition.tools, None);
    }

    #[test]
    fn rejects_an_unknown_frontmatter_key() {
        let raw = "---\nname: coder\ntimeout: 30\n---\nprompt\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentDefinition(_)));
        assert!(error.to_string().contains("timeout"), "{error}");
    }

    #[test]
    fn rejects_a_blank_but_present_tools_field() {
        let raw = "---\ntools: \"   \"\n---\nprompt\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentDefinition(_)));
        assert!(error.to_string().contains("tools"), "{error}");
    }

    #[test]
    fn rejects_a_blank_but_present_model_field() {
        let raw = "---\nmodel: \"\"\n---\nprompt\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentDefinition(_)));
        assert!(error.to_string().contains("model"), "{error}");
    }

    #[test]
    fn rejects_a_blank_system_prompt() {
        let empty_body = "---\nname: coder\n---\n";
        assert!(matches!(
            parse_agent_definition(empty_body),
            Err(CoreError::MalformedAgentDefinition(_))
        ));

        let whitespace_body = "---\nname: coder\n---\n  \n\t\n";
        assert!(matches!(
            parse_agent_definition(whitespace_body),
            Err(CoreError::MalformedAgentDefinition(_))
        ));

        let no_body_at_all = "---\nname: coder\n---";
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
        let raw = "---\nname: coder\nprompt\n";
        assert!(matches!(
            parse_agent_definition(raw),
            Err(CoreError::MalformedAgentDefinition(_))
        ));
    }

    #[test]
    fn a_legacy_toml_plus_fence_definition_is_rejected_not_the_new_dash_schema() {
        let raw =
            "+++\nrunner = \"command\"\nprogram = \"echo\"\nargs = [\"hi\"]\n+++\nbe an agent\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentDefinition(_)));
        assert!(error.to_string().contains("---"), "{error}");
    }

    #[test]
    fn old_warden_native_field_names_are_rejected_as_unknown_keys_under_the_new_fence() {
        let raw = "---\nrunner: command\nprogram: echo\n---\nbe an agent\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentDefinition(_)));
        assert!(error.to_string().contains("runner"), "{error}");
    }

    #[test]
    fn rejects_malformed_yaml_frontmatter() {
        let raw = "---\nname: [unterminated\n---\nprompt\n";
        assert!(matches!(
            parse_agent_definition(raw),
            Err(CoreError::MalformedAgentDefinition(_))
        ));
    }

    #[test]
    fn new_rejects_a_blank_system_prompt_like_the_parser_does() {
        assert!(matches!(
            AgentDefinition::new(None, None, None, None, "  \n\t"),
            Err(CoreError::MalformedAgentDefinition(_))
        ));
        assert_eq!(
            AgentDefinition::new(None, None, None, None, "  be a coder\n")
                .unwrap()
                .system_prompt,
            "be a coder"
        );
    }

    #[test]
    fn new_rejects_a_blank_but_present_optional_field_like_the_parser_does() {
        assert!(matches!(
            AgentDefinition::new(Some("   ".to_string()), None, None, None, "be a coder"),
            Err(CoreError::MalformedAgentDefinition(_))
        ));
    }

    #[test]
    fn a_crlf_definition_is_rejected_naming_the_line_endings_not_the_fence() {
        let raw = "---\r\nname: coder\r\n---\r\nprompt\r\n";
        let error = parse_agent_definition(raw).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("CRLF"), "{rendered}");
    }

    #[test]
    fn a_bom_prefixed_definition_is_rejected_naming_the_bom_not_the_fence() {
        let raw = "\u{feff}---\nname: coder\n---\nprompt\n";
        let error = parse_agent_definition(raw).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("byte order mark"), "{rendered}");
    }

    #[test]
    fn a_fence_like_line_inside_the_body_stays_part_of_the_prompt() {
        let raw = "---\nname: coder\n---\nprompt\n---\nmore prompt\n";
        let definition = parse_agent_definition(raw).unwrap();
        assert_eq!(definition.system_prompt, "prompt\n---\nmore prompt");
    }
}
