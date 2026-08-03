use std::path::{Path, PathBuf};

use warden_core::{parse_agent_definition, AgentDefinition};

use crate::error::{AgentDefinitionError, Result};

pub(crate) const AGENTS_DIR: &str = "agents";

pub async fn resolve_agent_definition(
    definitions_root: &Path,
    role_name: &str,
    agent_name: &str,
) -> Result<AgentDefinition> {
    let path = definition_path(definitions_root, agent_name);
    match tokio::fs::read_to_string(&path).await {
        Ok(raw) => parse_agent_definition(&raw)
            .map_err(|source| AgentDefinitionError::Invalid { path, source }.into()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            Err(AgentDefinitionError::CustomStepAgentNotFound {
                role: role_name.to_string(),
                expected_path: path,
            }
            .into())
        }
        Err(source) => Err(AgentDefinitionError::Read { path, source }.into()),
    }
}

fn definition_path(definitions_root: &Path, agent_name: &str) -> PathBuf {
    definitions_root
        .join(AGENTS_DIR)
        .join(format!("{agent_name}.md"))
}

#[derive(Debug, Clone)]
pub(crate) enum RawDefinition {
    Absent,
    Present(Vec<u8>),
    Unreadable {
        kind: std::io::ErrorKind,
        message: String,
    },
}

impl PartialEq for RawDefinition {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Absent, Self::Absent) => true,
            (Self::Present(left), Self::Present(right)) => left == right,
            (Self::Unreadable { kind: left, .. }, Self::Unreadable { kind: right, .. }) => {
                left == right
            }
            _ => false,
        }
    }
}

impl Eq for RawDefinition {}

pub(crate) async fn read_raw_definition(
    definitions_root: &Path,
    agent_name: &str,
) -> RawDefinition {
    let path = definition_path(definitions_root, agent_name);
    match tokio::fs::read(&path).await {
        Ok(bytes) => RawDefinition::Present(bytes),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => RawDefinition::Absent,
        Err(source) => RawDefinition::Unreadable {
            kind: source.kind(),
            message: source.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn resolves_any_named_agent_from_warden_directory() {
        let repo = TempDir::new().unwrap();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(
            repo.path().join(AGENTS_DIR).join("security.md"),
            "---\nmodel: opus\n---\nReview security.\n",
        )
        .await
        .unwrap();
        let definition = resolve_agent_definition(repo.path(), "audit", "security")
            .await
            .unwrap();
        assert_eq!(definition.system_prompt, "Review security.");
    }

    #[tokio::test]
    async fn missing_definition_is_error_without_fallback() {
        let repo = TempDir::new().unwrap();
        let error = resolve_agent_definition(repo.path(), "audit", "security")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("agents/security.md"));
    }
}
