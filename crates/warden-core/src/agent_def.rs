//! On-disk form of a **markdown agent definition** (issue #24): the file a
//! role's `--tool` adapter looks for by convention
//! (`<repo>/.warden/agents/{coder,reviewer,tester}.md`), which lets a role
//! override its identity -- name, description, allowed tools, model -- and
//! system prompt without the user hand-wiring any CLI invocation at all.
//!
//! A definition is a YAML frontmatter block fenced by [`FRONTMATTER_FENCE`],
//! followed by the markdown body -- the role's **system prompt**:
//!
//! ```text
//! ---
//! name: coder
//! description: Implements the task on the working branch.
//! tools: Read, Edit, Bash
//! model: sonnet
//! ---
//!
//! You are Warden's coder. Read the JSON payload on stdin ...
//! ```
//!
//! The schema is **Claude Code's own** `.claude/agents/*.md` subagent format
//! (issue #24, reversing ADR-0013 / Q1's warden-native TOML schema): adopting
//! it directly is what lets `warden run --tool claude` work against a
//! definition the user may already have lying around for Claude Code itself,
//! with zero markdown required at all when there is none. `name`/
//! `description` are accepted (never rejected as unknown keys) for exactly
//! that reason -- a real Claude Code subagent file always carries them --
//! even though Warden itself has no operational use for either today; only
//! `tools`/`model` feed into the invocation a
//! `warden::tool_adapter::ToolAdapter` builds. Warden-agnosticism (ADR-0005)
//! now lives one layer up, in the pluggable `ToolAdapter` seam, rather than
//! in the definition schema itself -- see `warden::tool_adapter`'s module
//! docs for the full rationale of that trade.
//!
//! Pure/parsing shape only -- reading the file off disk (and falling back to
//! an adapter's default prompt when no file is present) lives in
//! `warden::agent_def`, mirroring the `warden_core::agent_wire` /
//! `warden::process` split. Built here with the same "wire struct private +
//! public type + typed constructor + validated `parse_*` at the boundary"
//! convention as `agent_wire`/`ci_channel`/`evidence_wire`: an unknown key
//! and a blank system prompt are both typed errors, never silently defaulted
//! or accepted-then-ignored.

use serde::Deserialize;

use crate::error::{CoreError, Result};

/// Fences the YAML frontmatter block, opening and closing -- the same `---`
/// convention Claude Code's own `.claude/agents/*.md` subagent files use
/// (issue #24 / Q1, reversing ADR-0013's TOML `+++` fence): matching that
/// convention exactly, not just the field names, is what lets a definition
/// written for Claude Code double as a Warden one unchanged.
///
/// Private: the fence is an implementation detail of
/// [`parse_agent_definition`], which is the only way in. Callers parse
/// definitions, they don't assemble them.
const FRONTMATTER_FENCE: &str = "---";

/// The frontmatter's keys -- exactly Claude Code's own subagent schema
/// (`name`/`description`/`tools`/`model`), nothing warden-native added on
/// top. `deny_unknown_fields`: a key outside this set trips the same
/// accepted-then-ignored failure `code-standards.md` forbids everywhere else
/// in this codebase (ADR-0013 / Q3's precedent, carried over even though the
/// schema itself is no longer warden-native) -- a definition author who
/// thinks they configured something Warden silently dropped is exactly the
/// bug this guards against. `tools`/`model` are the two keys
/// `warden::tool_adapter::ToolAdapter::build_command` actually consumes;
/// `name`/`description` are accepted purely for Claude Code file
/// compatibility (see this module's docs) and otherwise unused.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontmatterWire {
    name: Option<String>,
    description: Option<String>,
    /// Claude Code's own format: a comma-or-space-separated tool-name list
    /// as a single string (matching `claude --allowedTools`'s own accepted
    /// input), never a YAML list -- deliberately kept a plain string rather
    /// than parsed into a `Vec` here, since `ToolAdapter::build_command`
    /// hands it straight through to the CLI flag verbatim.
    tools: Option<String>,
    /// A model alias (`"sonnet"`, `"opus"`, ...) or full model name, passed
    /// through verbatim to `--model` by the adapter. Never validated against
    /// a closed set here -- Warden has no fixed list of valid model names to
    /// validate against, and a wrong one is the underlying CLI's own error to
    /// report.
    model: Option<String>,
}

/// A parsed, validated markdown agent definition (issue #24).
///
/// Never built by hand from raw strings: [`AgentDefinition::new`] and
/// [`parse_agent_definition`] enforce the same invariants (a non-blank
/// system prompt, no blank-but-present optional field), following
/// `AgentInputMessage`'s convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    /// Accepted for Claude Code file compatibility; Warden has no
    /// operational use for it.
    pub name: Option<String>,
    /// Accepted for Claude Code file compatibility; Warden has no
    /// operational use for it.
    pub description: Option<String>,
    /// Passed to `--allowedTools`/equivalent by the tool adapter, verbatim.
    /// `None` here means only "this definition file did not itself set a
    /// `tools:` key" -- it does **not** mean "let the tool decide". Claude
    /// Code's own docs describe an omitted `tools:` key as "inherits every
    /// tool", but verified directly against the real CLI in non-interactive
    /// `-p` mode, `None`/no `--allowedTools` at all denies every mutating
    /// tool call outright (see `warden::tool_adapter::ClaudeAdapter`'s own
    /// docs). Because of that, `warden::agent_def::resolve_agent_definition`
    /// never lets a `None` here reach `ToolAdapter::build_command` as-is: a
    /// definition file that omits `tools` still has the adapter's own
    /// default grant merged in after parsing (issue #24 review finding B2 --
    /// a `tools: None` invocation silently muzzles the agent, which then
    /// raises/does nothing and produces a false convergence). This field
    /// being `None` on a freshly-`parse_agent_definition`d value only means
    /// "the file didn't say"; what an adapter actually does with that is the
    /// adapter's call, made once, at resolution time -- not here.
    pub tools: Option<String>,
    /// Passed to `--model`/equivalent by the tool adapter, verbatim. `None`
    /// means "let the tool pick its own default model".
    pub model: Option<String>,
    /// The markdown body after the frontmatter: what this role *is*. Fed to
    /// the underlying CLI as its system prompt (e.g.
    /// `claude --append-system-prompt`, ADR-0013 / Q2's "no argv" stance
    /// reversed for this concrete, tool-specific channel -- see
    /// `warden::tool_adapter` for why). Whitespace-trimmed, and never blank
    /// -- a definition whose body says nothing configures nothing.
    pub system_prompt: String,
}

impl AgentDefinition {
    /// Validates a definition's invariants at construction with the same
    /// rigor [`parse_agent_definition`] applies on the read side: a blank
    /// (empty or all-whitespace) system prompt is a typed error, and any
    /// *present* optional field that is blank/all-whitespace is rejected
    /// rather than silently treated as absent -- a definition author who
    /// wrote `tools: ""` meant something by it, even if what they meant is
    /// unclear, and guessing "they meant to omit it" is exactly the kind of
    /// silent normalization code-standards.md forbids.
    ///
    /// The prompt is stored trimmed -- leading/trailing blank lines around a
    /// markdown body are formatting, not content.
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

/// Shared guard for every optional frontmatter field: `None` (the key was
/// omitted entirely) is always fine, but `Some("")`/`Some("   ")` (the key
/// was present and blank) is a typed error naming the field, never silently
/// treated as if the key had been omitted.
fn reject_blank_if_present(field: &'static str, value: Option<String>) -> Result<Option<String>> {
    match value {
        Some(raw) if raw.trim().is_empty() => Err(CoreError::MalformedAgentDefinition(format!(
            "agent definition `{field}` must not be blank when present (omit the key entirely \
             to leave it unset)"
        ))),
        other => Ok(other),
    }
}

/// Parses one markdown agent definition, with the same rigor as
/// `parse_agent_input_message`/`parse_ci_result_message`: a missing or
/// unterminated frontmatter fence, malformed YAML, an unknown key, a blank
/// (but present) optional field, or a blank system prompt are all typed
/// errors -- never silently defaulted (code-standards.md: "valider toute
/// entrée externe ... à la frontière").
pub fn parse_agent_definition(raw: &str) -> Result<AgentDefinition> {
    let (frontmatter, body) = split_frontmatter(raw)?;
    let wire = parse_frontmatter(frontmatter)?;
    AgentDefinition::new(wire.name, wire.description, wire.tools, wire.model, body)
}

/// A UTF-8 BOM: legal in a text file, and invisible in an editor, but it
/// sits *before* the opening fence and would make the fence check fail while
/// the first line visibly reads `---`.
const BYTE_ORDER_MARK: &str = "\u{feff}";

/// Splits `---\n<frontmatter>\n---\n<body>` into its two halves.
///
/// Strict about both fences: a file that merely *looks* like a definition
/// (no fence at all, or an opening fence that is never closed) is rejected
/// naming what was expected, rather than being silently read as a
/// prompt-only file with default configuration.
fn split_frontmatter(raw: &str) -> Result<(&str, &str)> {
    // A CRLF or BOM file would otherwise fail the fence check below with
    // "must start with a `---` fence on its own first line" -- about a file
    // whose first line *is* visibly `---` (carried over from ADR-0013's own
    // review, LOW). Loud but actively misleading is its own bug, and a
    // `.gitattributes` `text eol=crlf` checkout makes it reachable. Named
    // explicitly instead; deliberately *not* normalised away, since silently
    // rewriting the input would also rewrite the system prompt (stray `\r`
    // in every line of the body) rather than just the fences.
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
    // An immediately-closing fence (empty frontmatter, e.g. `---\n---\n<body>`):
    // every frontmatter key is optional (issue #24), so a definition that
    // only wants to override the system prompt has nothing to put between
    // the fences at all. `rest` here has already had the opening `---\n`
    // stripped, so the closing fence is the very first thing in it -- no
    // leading `\n` for `closing_fence` above to match on.
    if let Some(body) = rest.strip_prefix(&format!("{FRONTMATTER_FENCE}\n")) {
        return Ok(("", body));
    }
    // Same case, but with no body at all either (`---\n---`, nothing past
    // the closing fence). Reported as the blank-prompt error it really is
    // (by `AgentDefinition::new`), not a bogus "missing closing fence".
    if rest == FRONTMATTER_FENCE {
        return Ok(("", ""));
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

/// Deserializes the frontmatter block into [`FrontmatterWire`]. An
/// all-blank block (`---\n---\n`, no keys at all) is treated as "every key
/// omitted" rather than run through the YAML parser -- `serde_yaml` rejects
/// an empty document when deserializing into a struct (it has no mapping to
/// deserialize at all), which would otherwise make the legitimate
/// "override only the system prompt, keep every adapter default" case a
/// parse error.
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

    /// Every frontmatter key is optional -- a definition that only wants to
    /// override the system prompt, leaving `tools`/`model` to the adapter's
    /// own defaults, needs no ceremony.
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

    /// A frontmatter block that is present but contains only blank lines
    /// must be treated identically to a wholly empty one.
    #[test]
    fn a_blank_but_present_frontmatter_block_is_treated_as_no_keys_at_all() {
        let raw = "---\n   \n\t\n---\nreview it\n";
        let definition = parse_agent_definition(raw).unwrap();
        assert_eq!(definition.name, None);
        assert_eq!(definition.tools, None);
    }

    /// Q3 precedent (ADR-0013), carried over to the new schema: unknown keys
    /// are rejected, naming the offending key -- silently ignoring one would
    /// run an agent configured differently than its author asked for.
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

    /// Issue #24 point 4 regression guard: ADR-0013's old warden-native
    /// schema fenced its frontmatter with `+++` (TOML), not `---` (YAML).
    /// That schema is fully gone -- a file still written in it must be
    /// rejected exactly like any other fence-less file, never partially
    /// understood or silently accepted as "no frontmatter, prompt-only".
    #[test]
    fn a_legacy_toml_plus_fence_definition_is_rejected_not_the_new_dash_schema() {
        let raw =
            "+++\nrunner = \"command\"\nprogram = \"echo\"\nargs = [\"hi\"]\n+++\nbe an agent\n";
        let error = parse_agent_definition(raw).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentDefinition(_)));
        // The error must name what it actually expected (the `---` fence),
        // not silently treat the `+++` line as if it were one.
        assert!(error.to_string().contains("---"), "{error}");
    }

    /// Issue #24 point 4, the other half: even under the *new* `---` fence,
    /// the old schema's own field names (`runner`/`program`/`args`) are not
    /// grandfathered in -- they trip `deny_unknown_fields` exactly like any
    /// other unrecognised key, naming the offending one.
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

    /// `new` enforces the same invariant the parser does, so a caller
    /// constructing a definition programmatically (a test fixture, an
    /// adapter's default-prompt fallback) can never produce one
    /// `parse_agent_definition` would refuse.
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

    /// LOW (carried over from ADR-0013's own review): a CRLF file's first
    /// line visibly *is* `---`, so "must start with a `---` fence" would be
    /// loud but actively misdirecting. The error must name the real cause --
    /// and the file must still be rejected, not silently normalised (that
    /// would rewrite the system prompt too, not just the fences).
    #[test]
    fn a_crlf_definition_is_rejected_naming_the_line_endings_not_the_fence() {
        let raw = "---\r\nname: coder\r\n---\r\nprompt\r\n";
        let error = parse_agent_definition(raw).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("CRLF"), "{rendered}");
    }

    /// Same misdirection, invisible cause: a BOM sits before the fence.
    #[test]
    fn a_bom_prefixed_definition_is_rejected_naming_the_bom_not_the_fence() {
        let raw = "\u{feff}---\nname: coder\n---\nprompt\n";
        let error = parse_agent_definition(raw).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("byte order mark"), "{rendered}");
    }

    /// A `---` inside the prompt body must not be mistaken for the fence --
    /// only the first closing fence terminates the frontmatter.
    #[test]
    fn a_fence_like_line_inside_the_body_stays_part_of_the_prompt() {
        let raw = "---\nname: coder\n---\nprompt\n---\nmore prompt\n";
        let definition = parse_agent_definition(raw).unwrap();
        assert_eq!(definition.system_prompt, "prompt\n---\nmore prompt");
    }
}
