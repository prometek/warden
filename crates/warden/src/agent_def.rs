//! Resolves a role's markdown agent definition (issue #24) -- by
//! **convention**, `<repo>/.warden/agents/{coder,reviewer,tester}.md`, or the
//! selected `--tool` adapter's own default prompt when no such file exists.
//! This is what makes `warden run --repo <path> --intent "..." --tool
//! claude` work with zero markdown at all: unlike the removed `--coder-
//! agent`/`--reviewer-agent`/`--tester-agent` flags (ADR-0013, issue #22),
//! there is no required path for the user to supply.
//!
//! I/O only: the schema, its validation, and every rule about what a
//! definition may say live in `warden_core::agent_def` (pure, testable
//! without a filesystem), mirroring the `warden_core::agent_wire` /
//! `warden::process` split. This module's job is "does the convention file
//! exist -- if so, read and validate it, naming the file on either failure;
//! if not, ask the adapter for its default".
//!
//! # Security: resolved once, from the run's base repo, before any agent runs
//!
//! [`resolve_agent_definition`] is called exactly once per role, against
//! `RunConfig::repo_path` (the run's **base** repository -- the path the
//! user passed to `--repo`), at the very start of
//! `Orchestrator::run_convergence_loop`, before the coder (or anyone else)
//! is spawned. The three resulting [`warden_core::AgentDefinition`]s are
//! then held in memory (`RunConfig`) for the run's entire lifetime -- they
//! are **never** re-read from a role's own worktree, or re-resolved
//! mid-run, no matter how many cycles a run goes through.
//!
//! This is deliberate, not an oversight: the coder is the one role that
//! writes to the repository, on a branch that becomes the reviewer's and
//! tester's own worktree once the coder commits (`WorktreeManager::create`
//! syncs each role's worktree onto the coder's latest commit every cycle,
//! see `orchestrator.rs`). If reviewer/tester definitions were re-read from
//! that same moving branch per invocation instead of snapshotted once at run
//! start, a coder -- buggy, or an agent that has gone off the rails under
//! prompt injection from the very code it's reviewing -- could commit a
//! rewritten `.warden/agents/reviewer.md` (a different system prompt, a
//! looser `model`, a `tools` list stripping away whatever safety
//! constraints the prompt relied on) and have its own reviewer or tester
//! evaluate it under rules the coder itself just chose. That would defeat
//! the entire point of running reviewer/tester as an independent gate
//! (Architecture.md §1, "Reviewer: ... remonte des findings de sécurité/
//! implémentation" -- independent of the coder's own judgement). Resolving
//! from the base repo, once, before the coder ever runs, keeps the review
//! gate's own configuration outside the coder's reach for the run's whole
//! duration, exactly like `warden-gated` never trusts a notification alone
//! without revalidating against its own read-only state (ADR-0006).
//!
//! This reading of the ticket's otherwise-unwritten security requirement
//! ("dépend des décisions de sécurité ci-dessous (isolation filesystem,
//! définitions coder-controllables)", issue #24 -- no such section exists in
//! the issue body, and there are no comments on it) is the conservative one
//! and should be confirmed with the issue's owner; it is not itself
//! something this module can verify at compile time or in a test.

use std::path::Path;

use warden_core::{parse_agent_definition, AgentDefinition, AgentRole};

use crate::error::{AgentDefinitionError, Result};
use crate::tool_adapter::ToolAdapter;

/// The directory, relative to a repo's root, that holds convention-based
/// agent definitions (issue #24 point 3): `.warden/agents/coder.md`,
/// `.warden/agents/reviewer.md`, `.warden/agents/tester.md`.
const AGENTS_DIR: &str = ".warden/agents";

/// Resolves `role`'s definition for this run: `<repo>/.warden/agents/
/// <role>.md` if present, else `adapter`'s own default prompt and `tools`
/// grant (issue #24 point 3). See this module's own docs for why this must
/// only ever be called once per role, at run start, against the base repo.
///
/// A convention file that exists but fails to read (permissions, not a
/// regular file, ...) or fails to parse (bad frontmatter, unknown key, blank
/// prompt, ...) is a typed error naming the path -- never silently treated
/// as "absent, fall back to the default". Only a **missing** file
/// (`io::ErrorKind::NotFound`) falls back; any other read failure is a
/// [`AgentDefinitionError::Read`] naming the path, exactly like a parse
/// failure is an [`AgentDefinitionError::Invalid`] naming it.
pub async fn resolve_agent_definition(
    repo_path: &Path,
    role: AgentRole,
    adapter: &impl ToolAdapter,
) -> Result<AgentDefinition> {
    let path = repo_path
        .join(AGENTS_DIR)
        .join(format!("{}.md", role.as_str()));

    match tokio::fs::read_to_string(&path).await {
        Ok(raw) => {
            Ok(
                parse_agent_definition(&raw).map_err(|source| AgentDefinitionError::Invalid {
                    path: path.clone(),
                    source,
                })?,
            )
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            // `AgentDefinition::new` enforces the exact same invariants
            // `parse_agent_definition` does (agent_def.rs), so an adapter's
            // own default prompt/tools can never produce a definition the
            // parser would have refused. `default_tools` matters as much as
            // `default_prompt` here -- see `ToolAdapter::default_tools`'s
            // own docs for why a `None` grant would leave the "zero .md" UX
            // unable to act at all for a tool like `claude`.
            Ok(AgentDefinition::new(
                None,
                None,
                adapter.default_tools(role).map(str::to_string),
                None,
                adapter.default_prompt(role),
            )?)
        }
        Err(source) => Err(AgentDefinitionError::Read { path, source }.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WardenError;
    use tempfile::TempDir;
    use warden_core::AgentRole;

    struct FakeAdapter;

    impl ToolAdapter for FakeAdapter {
        fn build_command(
            &self,
            _definition: &AgentDefinition,
        ) -> Result<crate::process::AgentCommand> {
            unreachable!("not exercised by these tests")
        }

        fn env_allowlist(&self) -> &'static [&'static str] {
            &[]
        }

        fn extract_findings(
            &self,
            _stdout: &str,
        ) -> warden_core::Result<Vec<warden_core::Finding>> {
            unreachable!("not exercised by these tests")
        }

        fn default_prompt(&self, role: AgentRole) -> &'static str {
            match role {
                AgentRole::Coder => "default coder prompt",
                AgentRole::Reviewer => "default reviewer prompt",
                AgentRole::Tester => "default tester prompt",
            }
        }

        fn default_tools(&self, _role: AgentRole) -> Option<&'static str> {
            None
        }
    }

    const DEFINITION: &str = "---\nmodel: opus\n---\n\nYou are Warden's reviewer.\n";

    #[tokio::test]
    async fn loads_and_validates_the_convention_file_when_present() {
        let repo = TempDir::new().unwrap();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(repo.path().join(AGENTS_DIR).join("reviewer.md"), DEFINITION)
            .await
            .unwrap();

        let definition = resolve_agent_definition(repo.path(), AgentRole::Reviewer, &FakeAdapter)
            .await
            .unwrap();

        assert_eq!(definition.model.as_deref(), Some("opus"));
        assert_eq!(definition.system_prompt, "You are Warden's reviewer.");
    }

    /// The zero-`.md` UX (issue #24): no convention file at all falls back
    /// to the adapter's own default prompt for that role, never an error.
    #[tokio::test]
    async fn falls_back_to_the_adapters_default_prompt_when_no_convention_file_exists() {
        let repo = TempDir::new().unwrap();

        let coder = resolve_agent_definition(repo.path(), AgentRole::Coder, &FakeAdapter)
            .await
            .unwrap();
        assert_eq!(coder.system_prompt, "default coder prompt");
        assert_eq!(coder.model, None);

        let tester = resolve_agent_definition(repo.path(), AgentRole::Tester, &FakeAdapter)
            .await
            .unwrap();
        assert_eq!(tester.system_prompt, "default tester prompt");
    }

    /// A convention file that exists but is unreadable for a reason other
    /// than "doesn't exist" (here: it's a directory, not a file) must not be
    /// silently treated as absent -- that would hide a real misconfiguration
    /// behind the adapter's default prompt.
    #[tokio::test]
    async fn a_convention_path_that_is_not_a_regular_file_is_a_read_error_not_a_fallback() {
        let repo = TempDir::new().unwrap();
        tokio::fs::create_dir_all(
            repo.path().join(AGENTS_DIR).join("coder.md"), // a directory, not a file
        )
        .await
        .unwrap();

        let error = resolve_agent_definition(repo.path(), AgentRole::Coder, &FakeAdapter)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Read { .. })
        ));
    }

    /// An invalid convention file must surface as `Invalid` (not silently
    /// fall back to the default), naming the file and the parser's own
    /// reason.
    #[tokio::test]
    async fn an_invalid_convention_file_is_a_typed_error_naming_the_path_and_the_reason() {
        let repo = TempDir::new().unwrap();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(
            repo.path().join(AGENTS_DIR).join("tester.md"),
            "no frontmatter here\n",
        )
        .await
        .unwrap();

        let error = resolve_agent_definition(repo.path(), AgentRole::Tester, &FakeAdapter)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Invalid { .. })
        ));
        let rendered = error.to_string();
        assert!(rendered.contains("tester.md"), "{rendered}");
        assert!(rendered.contains("frontmatter"), "{rendered}");
    }
}
