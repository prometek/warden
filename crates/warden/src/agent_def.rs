//! Reads a markdown agent definition off disk (ADR-0013, issue #22 Scope A)
//! -- the `--coder-agent`/`--reviewer-agent`/`--tester-agent` file.
//!
//! I/O only: the schema, its validation, and every rule about what a
//! definition may say live in `warden_core::agent_def` (pure, testable
//! without a filesystem), mirroring the `warden_core::agent_wire` /
//! `warden::process` split. This module's whole job is "bytes off disk, then
//! hand them to the boundary parser, naming the file in either failure" --
//! an unreadable or invalid definition must be actionable without the user
//! having to guess which of the three files was at fault.

use std::path::Path;

use warden_core::{parse_agent_definition, AgentDefinition};

use crate::error::{AgentDefinitionError, Result};

/// Loads and validates the agent definition at `path`.
///
/// Both failure modes are typed and carry the path: the file couldn't be
/// read at all ([`AgentDefinitionError::Read`]), or it was read but isn't a
/// valid definition ([`AgentDefinitionError::Invalid`]). Never a partial or
/// defaulted definition -- there is no such thing as "most of an agent".
pub async fn load_agent_definition(path: &Path) -> Result<AgentDefinition> {
    let raw =
        tokio::fs::read_to_string(path)
            .await
            .map_err(|source| AgentDefinitionError::Read {
                path: path.to_path_buf(),
                source,
            })?;

    Ok(
        parse_agent_definition(&raw).map_err(|source| AgentDefinitionError::Invalid {
            path: path.to_path_buf(),
            source,
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WardenError;
    use tempfile::TempDir;
    use warden_core::RunnerKind;

    const DEFINITION: &str = r#"+++
runner = "command"
program = "sh"
args = ["-c", "true"]
+++

You are Warden's reviewer.
"#;

    #[tokio::test]
    async fn loads_and_validates_a_definition_from_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("reviewer.md");
        tokio::fs::write(&path, DEFINITION).await.unwrap();

        let definition = load_agent_definition(&path).await.unwrap();

        assert_eq!(
            definition.runner,
            RunnerKind::Command {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), "true".to_string()],
            }
        );
        assert_eq!(definition.system_prompt, "You are Warden's reviewer.");
    }

    /// A missing file is a typed error naming the path -- the dominant real
    /// misuse (a typo in `--coder-agent`), and useless if it just says "no
    /// such file".
    #[tokio::test]
    async fn a_missing_definition_file_is_a_typed_error_naming_the_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.md");

        let error = load_agent_definition(&path).await.unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Read { .. })
        ));
        assert!(error.to_string().contains("nope.md"), "{error}");
    }

    /// An invalid definition must surface as `Invalid` (not `Read`), still
    /// naming the file *and* preserving the core parser's own reason.
    #[tokio::test]
    async fn an_invalid_definition_is_a_typed_error_naming_the_path_and_the_reason() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("coder.md");
        tokio::fs::write(&path, "no frontmatter here\n")
            .await
            .unwrap();

        let error = load_agent_definition(&path).await.unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Invalid { .. })
        ));
        let rendered = error.to_string();
        assert!(rendered.contains("coder.md"), "{rendered}");
        assert!(rendered.contains("frontmatter"), "{rendered}");
    }
}
