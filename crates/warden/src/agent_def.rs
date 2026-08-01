use std::path::{Path, PathBuf};

use warden_core::{parse_agent_definition, AgentDefinition, AgentRole};

use crate::error::{AgentDefinitionError, Result};
use crate::path_util::canonicalize_best_effort;
use crate::tool_adapter::ToolAdapter;

pub(crate) const AGENTS_DIR: &str = ".warden/agents";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDefinitionSource {
    /// Coder only: `<repo>/.warden/agents/coder.md`.
    RepoConvention(PathBuf),
    UserConfig(PathBuf),
    UntrustedRepoOverride {
        /// The literal, pre-canonicalization path that was actually read.
        path: PathBuf,
        canonical_path: PathBuf,
    },
    /// No file at any location this role consults -- the selected tool adapter's own default
    /// prompt/tools.
    AdapterDefault,
}

pub async fn resolve_agent_definition(
    repo_path: &Path,
    role: AgentRole,
    adapter: &impl ToolAdapter,
    user_config_agents_dir: &Path,
    warden_home: &Path,
    trust_repo_agents: bool,
) -> Result<(AgentDefinition, AgentDefinitionSource)> {
    match role {
        AgentRole::Coder => {
            let path = repo_path
                .join(AGENTS_DIR)
                .join(format!("{}.md", role.as_str()));
            match try_read_definition(&path, role, adapter).await? {
                Some(definition) => Ok((definition, AgentDefinitionSource::RepoConvention(path))),
                None => Ok((
                    adapter_default_definition(role, adapter)?,
                    AgentDefinitionSource::AdapterDefault,
                )),
            }
        }
        AgentRole::Reviewer | AgentRole::Tester => {
            let user_config_path = user_config_agents_dir.join(format!("{}.md", role.as_str()));
            let warden_home_worktrees_root = warden_home.join("worktrees");

            match user_config_resolves_inside_repo_or_worktrees(
                user_config_agents_dir,
                &user_config_path,
                repo_path,
                &warden_home_worktrees_root,
            )? {
                None => {
                    if let Some(definition) =
                        try_read_definition(&user_config_path, role, adapter).await?
                    {
                        return Ok((
                            definition,
                            AgentDefinitionSource::UserConfig(user_config_path),
                        ));
                    }
                }
                Some(canonical_user_config_path) => {
                    if let Some(result) = try_untrusted_repo_source(
                        role,
                        &user_config_path,
                        adapter,
                        trust_repo_agents,
                        Some(&canonical_user_config_path),
                    )
                    .await?
                    {
                        return Ok(result);
                    }
                }
            }

            let repo_override_path = repo_path
                .join(AGENTS_DIR)
                .join(format!("{}.md", role.as_str()));
            if let Some(result) = try_untrusted_repo_source(
                role,
                &repo_override_path,
                adapter,
                trust_repo_agents,
                None,
            )
            .await?
            {
                return Ok(result);
            }

            Ok((
                adapter_default_definition(role, adapter)?,
                AgentDefinitionSource::AdapterDefault,
            ))
        }
    }
}

fn user_config_resolves_inside_repo_or_worktrees(
    user_config_agents_dir: &Path,
    user_config_path: &Path,
    repo_path: &Path,
    warden_home_worktrees_root: &Path,
) -> Result<Option<PathBuf>> {
    let canonical_repo = canonicalize_best_effort(repo_path).map_err(|source| {
        AgentDefinitionError::PathResolutionFailed {
            path: repo_path.to_path_buf(),
            source,
        }
    })?;
    let canonical_worktrees_root =
        canonicalize_best_effort(warden_home_worktrees_root).map_err(|source| {
            AgentDefinitionError::PathResolutionFailed {
                path: warden_home_worktrees_root.to_path_buf(),
                source,
            }
        })?;
    let canonical_dir = canonicalize_best_effort(user_config_agents_dir).map_err(|source| {
        AgentDefinitionError::PathResolutionFailed {
            path: user_config_agents_dir.to_path_buf(),
            source,
        }
    })?;
    let canonical_path = canonicalize_best_effort(user_config_path).map_err(|source| {
        AgentDefinitionError::PathResolutionFailed {
            path: user_config_path.to_path_buf(),
            source,
        }
    })?;

    let degraded = canonical_dir.starts_with(&canonical_repo)
        || canonical_path.starts_with(&canonical_repo)
        || canonical_dir.starts_with(&canonical_worktrees_root)
        || canonical_path.starts_with(&canonical_worktrees_root);

    Ok(degraded.then_some(canonical_path))
}

async fn try_untrusted_repo_source(
    role: AgentRole,
    candidate_path: &Path,
    adapter: &impl ToolAdapter,
    trust_repo_agents: bool,
    degraded_user_config_canonical_path: Option<&Path>,
) -> Result<Option<(AgentDefinition, AgentDefinitionSource)>> {
    if !trust_repo_agents {
        let exists = tokio::fs::try_exists(candidate_path)
            .await
            .map_err(|source| AgentDefinitionError::Read {
                path: candidate_path.to_path_buf(),
                source,
            })?;
        if exists {
            match degraded_user_config_canonical_path {
                Some(canonical) => tracing::warn!(
                    role = role.as_str(),
                    path = %candidate_path.display(),
                    resolves_to = %canonical.display(),
                    "ignoring a reviewer/tester definition that looked like the trusted user \
                     config source but actually resolves inside the repo under review or this \
                     warden home's own worktrees (see `resolves_to`), both of which the coder \
                     can write to; point XDG_CONFIG_HOME/HOME at a directory genuinely outside \
                     both, or pass --trust-repo-agents to use it as-is (untrusted)"
                ),
                None => tracing::warn!(
                    role = role.as_str(),
                    path = %candidate_path.display(),
                    "ignoring a repo-controlled agent definition for an independent role; move it \
                     to $XDG_CONFIG_HOME/warden/agents/ (or ~/.config/warden/agents/) to use it as \
                     the trusted source, or pass --trust-repo-agents to use it as-is (untrusted)"
                ),
            }
        }
        return Ok(None);
    }

    match try_read_definition(candidate_path, role, adapter).await? {
        Some(definition) => {
            tracing::warn!(
                role = role.as_str(),
                path = %candidate_path.display(),
                "using a repo-controlled agent definition for an independent role \
                 (--trust-repo-agents); this file is committable by the coder and is NOT \
                 trusted the way a genuine user-config definition is"
            );
            let canonical_path = match degraded_user_config_canonical_path {
                Some(canonical) => canonical.to_path_buf(),
                None => canonicalize_best_effort(candidate_path).map_err(|source| {
                    AgentDefinitionError::PathResolutionFailed {
                        path: candidate_path.to_path_buf(),
                        source,
                    }
                })?,
            };
            Ok(Some((
                definition,
                AgentDefinitionSource::UntrustedRepoOverride {
                    path: candidate_path.to_path_buf(),
                    canonical_path,
                },
            )))
        }
        None => Ok(None),
    }
}

/// Attempts to read, parse, and default-fill (B2) the definition at `path`.
async fn try_read_definition(
    path: &Path,
    role: AgentRole,
    adapter: &impl ToolAdapter,
) -> Result<Option<AgentDefinition>> {
    match tokio::fs::read_to_string(path).await {
        Ok(raw) => {
            let definition =
                parse_agent_definition(&raw).map_err(|source| AgentDefinitionError::Invalid {
                    path: path.to_path_buf(),
                    source,
                })?;
            Ok(Some(apply_default_tools_when_unset(
                definition, role, adapter,
            )))
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(AgentDefinitionError::Read {
            path: path.to_path_buf(),
            source,
        }
        .into()),
    }
}

/// The selected adapter's own default prompt/tools for `role`, used at every consulted location
/// once none of them has a file.
fn adapter_default_definition(
    role: AgentRole,
    adapter: &impl ToolAdapter,
) -> Result<AgentDefinition> {
    Ok(AgentDefinition::new(
        None,
        None,
        adapter.default_tools(role).map(str::to_string),
        None,
        adapter.default_prompt(role),
    )?)
}

pub fn default_user_config_agents_dir() -> Result<PathBuf> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => {
            let home = std::env::var("HOME").map_err(|_| {
                AgentDefinitionError::UserConfigDirUnresolvable {
                    reason: "neither XDG_CONFIG_HOME nor HOME is set".to_string(),
                }
            })?;
            if home.trim().is_empty() {
                return Err(AgentDefinitionError::UserConfigDirUnresolvable {
                    reason: "HOME is set but empty".to_string(),
                }
                .into());
            }
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("warden").join("agents"))
}

pub(crate) const CUSTOM_STEP_AGENTS_DIR: &str = ".claude/agents";

pub async fn resolve_custom_step_agent_definition(
    repo_path: &Path,
    role_name: &str,
    agent_name: &str,
) -> Result<AgentDefinition> {
    let path = repo_path
        .join(CUSTOM_STEP_AGENTS_DIR)
        .join(format!("{agent_name}.md"));
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
            Err(AgentDefinitionError::CustomStepAgentNotFound {
                role: role_name.to_string(),
                expected_path: path,
            }
            .into())
        }
        Err(source) => Err(AgentDefinitionError::Read { path, source }.into()),
    }
}

#[derive(Debug, Clone)]
pub(crate) enum RawDefinition {
    /// No file at the resolved path (`io::ErrorKind::NotFound`).
    Absent,
    /// The file's raw bytes, exactly as the OS returned them -- valid or invalid UTF-8, parsable
    /// frontmatter or not.
    Present(Vec<u8>),
    /// The path resolved to *something*, but the OS refused to read it for a reason other than
    /// "missing" (permission denied, a directory sitting at the path,...).
    Unreadable {
        kind: std::io::ErrorKind,
        message: String,
    },
}

impl PartialEq for RawDefinition {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Absent, Self::Absent) => true,
            (Self::Present(a), Self::Present(b)) => a == b,
            (Self::Unreadable { kind: a, .. }, Self::Unreadable { kind: b, .. }) => a == b,
            _ => false,
        }
    }
}

impl Eq for RawDefinition {}

pub(crate) async fn read_raw_definition(repo_path: &Path, role: AgentRole) -> RawDefinition {
    let path = repo_path
        .join(AGENTS_DIR)
        .join(format!("{}.md", role.as_str()));

    match tokio::fs::read(&path).await {
        Ok(bytes) => RawDefinition::Present(bytes),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => RawDefinition::Absent,
        Err(source) => RawDefinition::Unreadable {
            kind: source.kind(),
            message: source.to_string(),
        },
    }
}

fn apply_default_tools_when_unset(
    definition: AgentDefinition,
    role: AgentRole,
    adapter: &impl ToolAdapter,
) -> AgentDefinition {
    if definition.tools.is_some() {
        return definition;
    }
    AgentDefinition {
        tools: adapter.default_tools(role).map(str::to_string),
        ..definition
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
            Some("fake-default-tools")
        }
    }

    const DEFINITION: &str = "---\nmodel: opus\n---\n\nYou are Warden's reviewer.\n";

    async fn capture_tracing_output<T>(
        future: impl std::future::Future<Output = T>,
    ) -> (T, String) {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
            type Writer = SharedBuf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .finish();

        let guard = tracing::subscriber::set_default(subscriber);
        let result = future.await;
        drop(guard);

        let output = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        (result, output)
    }

    fn no_user_config_dir() -> TempDir {
        TempDir::new().unwrap()
    }

    fn no_warden_home_worktrees() -> TempDir {
        TempDir::new().unwrap()
    }

    #[tokio::test]
    async fn coder_loads_and_validates_the_repo_convention_file_when_present() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(repo.path().join(AGENTS_DIR).join("coder.md"), DEFINITION)
            .await
            .unwrap();

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.model.as_deref(), Some("opus"));
        assert_eq!(definition.system_prompt, "You are Warden's reviewer.");
        assert!(matches!(source, AgentDefinitionSource::RepoConvention(_)));
    }

    #[tokio::test]
    async fn coder_a_prompt_only_definition_still_gets_the_adapters_default_tools_grant() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(
            repo.path().join(AGENTS_DIR).join("coder.md"),
            "---\n---\nimplement it\n",
        )
        .await
        .unwrap();

        let (definition, _source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.tools.as_deref(), Some("fake-default-tools"));
        assert_eq!(definition.system_prompt, "implement it");
    }

    #[tokio::test]
    async fn coder_a_definition_that_sets_tools_explicitly_keeps_its_own_value() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(
            repo.path().join(AGENTS_DIR).join("coder.md"),
            "---\ntools: Read, Edit\n---\nimplement it\n",
        )
        .await
        .unwrap();

        let (definition, _source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.tools.as_deref(), Some("Read, Edit"));
    }

    #[tokio::test]
    async fn coder_falls_back_to_the_adapters_default_prompt_when_no_convention_file_exists() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();

        let (coder, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(coder.system_prompt, "default coder prompt");
        assert_eq!(coder.model, None);
        assert_eq!(source, AgentDefinitionSource::AdapterDefault);
    }

    #[tokio::test]
    async fn coder_convention_path_that_is_not_a_regular_file_is_a_read_error_not_a_fallback() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();
        tokio::fs::create_dir_all(
            repo.path().join(AGENTS_DIR).join("coder.md"), // a directory, not a file
        )
        .await
        .unwrap();

        let error = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Read { .. })
        ));
    }

    #[tokio::test]
    async fn coder_invalid_convention_file_is_a_typed_error_naming_the_path_and_the_reason() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(
            repo.path().join(AGENTS_DIR).join("coder.md"),
            "no frontmatter here\n",
        )
        .await
        .unwrap();

        let error = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Invalid { .. })
        ));
        let rendered = error.to_string();
        assert!(rendered.contains("coder.md"), "{rendered}");
        assert!(rendered.contains("frontmatter"), "{rendered}");
    }

    #[tokio::test]
    async fn coder_resolution_ignores_the_user_config_dir_entirely() {
        let repo = TempDir::new().unwrap();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(repo.path().join(AGENTS_DIR).join("coder.md"), DEFINITION)
            .await
            .unwrap();

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            Path::new(""),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.model.as_deref(), Some("opus"));
        assert_eq!(definition.system_prompt, "You are Warden's reviewer.");
        assert!(matches!(source, AgentDefinitionSource::RepoConvention(_)));
    }

    #[tokio::test]
    async fn coder_resolution_ignores_a_user_config_dir_that_does_contain_a_coder_md() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(user_config.path(), AgentRole::Coder, DEFINITION).await;

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.system_prompt, "default coder prompt");
        assert_eq!(definition.model, None);
        assert_eq!(source, AgentDefinitionSource::AdapterDefault);
    }

    async fn write_definition(dir: &Path, role: AgentRole, body: &str) {
        tokio::fs::create_dir_all(dir).await.unwrap();
        tokio::fs::write(dir.join(format!("{}.md", role.as_str())), body)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reviewer_and_tester_load_the_user_config_file_when_present() {
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            let repo = TempDir::new().unwrap();
            let user_config = TempDir::new().unwrap();
            write_definition(user_config.path(), role, DEFINITION).await;

            let (definition, source) = resolve_agent_definition(
                repo.path(),
                role,
                &FakeAdapter,
                user_config.path(),
                no_warden_home_worktrees().path(),
                false,
            )
            .await
            .unwrap();

            assert_eq!(definition.model.as_deref(), Some("opus"));
            assert_eq!(definition.system_prompt, "You are Warden's reviewer.");
            assert!(matches!(source, AgentDefinitionSource::UserConfig(_)));
        }
    }

    #[tokio::test]
    async fn reviewer_and_tester_ignore_the_repo_convention_file_by_default_but_warn_about_it() {
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            let repo = TempDir::new().unwrap();
            let user_config = no_user_config_dir();
            write_definition(&repo.path().join(AGENTS_DIR), role, "no frontmatter here\n").await;

            let (result, logs) = capture_tracing_output(resolve_agent_definition(
                repo.path(),
                role,
                &FakeAdapter,
                user_config.path(),
                no_warden_home_worktrees().path(),
                false,
            ))
            .await;
            let (definition, source) = result.unwrap();

            assert_eq!(source, AgentDefinitionSource::AdapterDefault);
            assert_eq!(
                definition.system_prompt,
                if role == AgentRole::Reviewer {
                    "default reviewer prompt"
                } else {
                    "default tester prompt"
                }
            );

            let expected_path = repo
                .path()
                .join(AGENTS_DIR)
                .join(format!("{}.md", role.as_str()));
            assert!(
                logs.contains("ignoring a repo-controlled agent definition"),
                "{logs:?}"
            );
            assert!(
                logs.contains(&expected_path.display().to_string()),
                "{logs:?}"
            );
        }
    }

    #[tokio::test]
    async fn trust_repo_agents_falls_back_to_the_repo_file_when_no_user_config_file_exists() {
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            let repo = TempDir::new().unwrap();
            let user_config = no_user_config_dir();
            write_definition(&repo.path().join(AGENTS_DIR), role, DEFINITION).await;

            let (definition, source) = resolve_agent_definition(
                repo.path(),
                role,
                &FakeAdapter,
                user_config.path(),
                no_warden_home_worktrees().path(),
                true,
            )
            .await
            .unwrap();

            assert_eq!(definition.system_prompt, "You are Warden's reviewer.");
            let expected_path = repo
                .path()
                .join(AGENTS_DIR)
                .join(format!("{}.md", role.as_str()));
            assert_eq!(
                source,
                AgentDefinitionSource::UntrustedRepoOverride {
                    path: expected_path.clone(),
                    canonical_path: expected_path.canonicalize().unwrap(),
                }
            );
        }
    }

    #[tokio::test]
    async fn user_config_file_wins_over_the_repo_file_even_with_trust_repo_agents() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            user_config.path(),
            AgentRole::Reviewer,
            "---\n---\nfrom user config\n",
        )
        .await;
        write_definition(
            &repo.path().join(AGENTS_DIR),
            AgentRole::Reviewer,
            "---\n---\nfrom the repo\n",
        )
        .await;

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            true,
        )
        .await
        .unwrap();

        assert_eq!(definition.system_prompt, "from user config");
        assert!(matches!(source, AgentDefinitionSource::UserConfig(_)));
    }

    #[tokio::test]
    async fn trust_repo_agents_still_falls_back_to_the_adapter_default_when_nothing_exists() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Tester,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            true,
        )
        .await
        .unwrap();

        assert_eq!(definition.system_prompt, "default tester prompt");
        assert_eq!(source, AgentDefinitionSource::AdapterDefault);
    }

    #[tokio::test]
    async fn reviewer_user_config_path_that_is_not_a_regular_file_is_a_read_error() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        tokio::fs::create_dir_all(user_config.path().join("reviewer.md")) // a directory
            .await
            .unwrap();

        let error = resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Read { .. })
        ));
    }

    #[tokio::test]
    async fn reviewer_invalid_user_config_file_is_a_typed_error_naming_the_path_and_the_reason() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            user_config.path(),
            AgentRole::Tester,
            "no frontmatter here\n",
        )
        .await;

        let error = resolve_agent_definition(
            repo.path(),
            AgentRole::Tester,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
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

    #[tokio::test]
    async fn reviewer_user_config_prompt_only_definition_still_gets_the_adapters_default_tools() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            user_config.path(),
            AgentRole::Reviewer,
            "---\n---\nreview it\n",
        )
        .await;

        let (definition, _source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.tools.as_deref(), Some("fake-default-tools"));
        assert_eq!(definition.system_prompt, "review it");
    }

    #[tokio::test]
    async fn a_user_config_dir_resolving_inside_the_repo_is_ignored_by_default_and_warns_with_the_degraded_message(
    ) {
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            let repo = TempDir::new().unwrap();
            let malicious_user_config_dir = repo.path().join(".config");
            write_definition(
                &malicious_user_config_dir.join("warden").join("agents"),
                role,
                "---\n---\nfrom the fake user config (actually the repo)\n",
            )
            .await;

            let (result, logs) = capture_tracing_output(resolve_agent_definition(
                repo.path(),
                role,
                &FakeAdapter,
                &malicious_user_config_dir.join("warden").join("agents"),
                no_warden_home_worktrees().path(),
                false,
            ))
            .await;
            let (definition, source) = result.unwrap();

            assert_eq!(source, AgentDefinitionSource::AdapterDefault);
            assert_eq!(
                definition.system_prompt,
                if role == AgentRole::Reviewer {
                    "default reviewer prompt"
                } else {
                    "default tester prompt"
                },
                "a degraded user-config source must never be read as trusted, even though a \
                 file genuinely exists there"
            );
            assert!(
                logs.contains(
                    "ignoring a reviewer/tester definition that looked like the trusted user \
                     config source"
                ),
                "{logs:?}"
            );
            assert!(
                !logs.contains("move it to $XDG_CONFIG_HOME/warden/agents/"),
                "the degraded-user-config case must not get the plain repo-convention advice, \
                 which is a no-op for a file already at that exact location: {logs:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_user_config_dir_resolving_inside_the_repo_is_used_as_untrusted_when_trusted() {
        let repo = TempDir::new().unwrap();
        let malicious_user_config_dir = repo.path().join(".config");
        let malicious_agents_dir = malicious_user_config_dir.join("warden").join("agents");
        write_definition(
            &malicious_agents_dir,
            AgentRole::Reviewer,
            "---\n---\nfrom the fake user config (actually the repo)\n",
        )
        .await;

        let (result, logs) = capture_tracing_output(resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            &malicious_agents_dir,
            no_warden_home_worktrees().path(),
            true,
        ))
        .await;
        let (definition, source) = result.unwrap();

        assert_eq!(
            definition.system_prompt,
            "from the fake user config (actually the repo)"
        );
        let expected_path = malicious_agents_dir.join("reviewer.md");
        assert_eq!(
            source,
            AgentDefinitionSource::UntrustedRepoOverride {
                path: expected_path.clone(),
                canonical_path: expected_path.canonicalize().unwrap(),
            }
        );
        assert!(logs.contains("NOT trusted"), "{logs:?}");
        assert!(
            logs.contains(&expected_path.display().to_string()),
            "{logs:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_user_config_file_pointing_into_the_repo_is_degraded_to_untrusted() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            &repo.path().join(AGENTS_DIR),
            AgentRole::Reviewer,
            "---\n---\nfrom the repo, via a symlink\n",
        )
        .await;

        std::os::unix::fs::symlink(
            repo.path().join(AGENTS_DIR).join("reviewer.md"),
            user_config.path().join("reviewer.md"),
        )
        .unwrap();

        let (result, logs) = capture_tracing_output(resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            true,
        ))
        .await;
        let (definition, source) = result.unwrap();

        assert_eq!(definition.system_prompt, "from the repo, via a symlink");
        match &source {
            AgentDefinitionSource::UntrustedRepoOverride {
                path,
                canonical_path,
            } => {
                assert_eq!(path, &user_config.path().join("reviewer.md"));
                assert_eq!(
                    canonical_path,
                    &repo
                        .path()
                        .join(AGENTS_DIR)
                        .join("reviewer.md")
                        .canonicalize()
                        .unwrap()
                );
            }
            other => panic!("expected UntrustedRepoOverride, got {other:?}"),
        }
        assert!(logs.contains("NOT trusted"), "{logs:?}");
    }

    #[tokio::test]
    async fn a_user_config_dir_genuinely_outside_the_repo_is_unaffected_by_the_containment_check() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            user_config.path(),
            AgentRole::Reviewer,
            "---\n---\ngenuinely trusted\n",
        )
        .await;

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.system_prompt, "genuinely trusted");
        assert!(matches!(source, AgentDefinitionSource::UserConfig(_)));
    }

    #[tokio::test]
    async fn a_user_config_dir_resolving_inside_warden_homes_worktrees_is_ignored_by_default_and_warns(
    ) {
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            let repo = TempDir::new().unwrap();
            let warden_home = TempDir::new().unwrap();
            let stale_worktree_user_config = warden_home
                .path()
                .join("worktrees")
                .join("crashed-run-id")
                .join("coder")
                .join(".config");
            write_definition(
                &stale_worktree_user_config.join("warden").join("agents"),
                role,
                "---\n---\nfrom a stale worktree, not the trusted user config\n",
            )
            .await;

            let (result, logs) = capture_tracing_output(resolve_agent_definition(
                repo.path(),
                role,
                &FakeAdapter,
                &stale_worktree_user_config.join("warden").join("agents"),
                warden_home.path(),
                false,
            ))
            .await;
            let (definition, source) = result.unwrap();

            assert_eq!(
                source,
                AgentDefinitionSource::AdapterDefault,
                "a user-config source resolving inside <warden_home>/worktrees/ must never be \
                 read as trusted, even though a file genuinely exists there"
            );
            assert_eq!(
                definition.system_prompt,
                if role == AgentRole::Reviewer {
                    "default reviewer prompt"
                } else {
                    "default tester prompt"
                }
            );
            assert!(
                logs.contains(
                    "ignoring a reviewer/tester definition that looked like the trusted user \
                     config source"
                ),
                "{logs:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_user_config_dir_resolving_inside_warden_homes_worktrees_is_used_as_untrusted_when_trusted(
    ) {
        let repo = TempDir::new().unwrap();
        let warden_home = TempDir::new().unwrap();
        let stale_worktree_user_config = warden_home
            .path()
            .join("worktrees")
            .join("crashed-run-id")
            .join("coder")
            .join(".config");
        let stale_agents_dir = stale_worktree_user_config.join("warden").join("agents");
        write_definition(
            &stale_agents_dir,
            AgentRole::Reviewer,
            "---\n---\nfrom a stale worktree, not the trusted user config\n",
        )
        .await;

        let (result, logs) = capture_tracing_output(resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            &stale_agents_dir,
            warden_home.path(),
            true,
        ))
        .await;
        let (definition, source) = result.unwrap();

        assert_eq!(
            definition.system_prompt,
            "from a stale worktree, not the trusted user config"
        );
        let expected_path = stale_agents_dir.join("reviewer.md");
        assert_eq!(
            source,
            AgentDefinitionSource::UntrustedRepoOverride {
                path: expected_path.clone(),
                canonical_path: expected_path.canonicalize().unwrap(),
            }
        );
        assert!(logs.contains("NOT trusted"), "{logs:?}");
    }

    #[tokio::test]
    async fn a_user_config_dir_outside_this_warden_homes_worktrees_is_unaffected() {
        let repo = TempDir::new().unwrap();
        let warden_home = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            user_config.path(),
            AgentRole::Reviewer,
            "---\n---\ngenuinely trusted\n",
        )
        .await;

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            warden_home.path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.system_prompt, "genuinely trusted");
        assert!(matches!(source, AgentDefinitionSource::UserConfig(_)));
    }

    #[tokio::test]
    async fn read_raw_definition_is_absent_when_no_convention_file_exists() {
        let repo = TempDir::new().unwrap();

        let raw = read_raw_definition(repo.path(), AgentRole::Reviewer).await;

        assert_eq!(raw, RawDefinition::Absent);
    }

    #[tokio::test]
    async fn read_raw_definition_returns_the_exact_bytes_even_when_not_parsable() {
        let repo = TempDir::new().unwrap();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        let poisoned: &[u8] = b"not even close to valid frontmatter \xff\xfe";
        tokio::fs::write(repo.path().join(AGENTS_DIR).join("reviewer.md"), poisoned)
            .await
            .unwrap();

        let raw = read_raw_definition(repo.path(), AgentRole::Reviewer).await;

        assert_eq!(raw, RawDefinition::Present(poisoned.to_vec()));
    }

    #[tokio::test]
    async fn read_raw_definition_is_unreadable_not_err_when_the_path_is_a_directory() {
        let repo = TempDir::new().unwrap();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR).join("coder.md"))
            .await
            .unwrap();

        let raw = read_raw_definition(repo.path(), AgentRole::Coder).await;

        assert!(matches!(raw, RawDefinition::Unreadable { .. }));
    }

    #[test]
    fn raw_definition_unreadable_compares_by_kind_not_by_message() {
        let a = RawDefinition::Unreadable {
            kind: std::io::ErrorKind::PermissionDenied,
            message: "Permission denied (os error 13)".to_string(),
        };
        let b = RawDefinition::Unreadable {
            kind: std::io::ErrorKind::PermissionDenied,
            message: "permission non accordée (erreur 13)".to_string(),
        };
        assert_eq!(
            a, b,
            "same ErrorKind must compare equal regardless of message text"
        );

        let different_kind = RawDefinition::Unreadable {
            kind: std::io::ErrorKind::IsADirectory,
            message: "Permission denied (os error 13)".to_string(),
        };
        assert_ne!(
            a, different_kind,
            "a different ErrorKind must never compare equal, even with the same message text"
        );
    }

    #[tokio::test]
    async fn resolves_a_custom_steps_definition_from_claude_agents_dir() {
        let repo = TempDir::new().unwrap();
        tokio::fs::create_dir_all(repo.path().join(CUSTOM_STEP_AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(
            repo.path().join(CUSTOM_STEP_AGENTS_DIR).join("techlead.md"),
            DEFINITION,
        )
        .await
        .unwrap();

        let definition = resolve_custom_step_agent_definition(repo.path(), "techlead", "techlead")
            .await
            .unwrap();

        assert_eq!(definition.model.as_deref(), Some("opus"));
        assert_eq!(definition.system_prompt, "You are Warden's reviewer.");
    }

    #[tokio::test]
    async fn a_missing_custom_step_definition_is_a_typed_error_naming_the_role_and_path() {
        let repo = TempDir::new().unwrap();

        let error = resolve_custom_step_agent_definition(repo.path(), "techlead", "techlead")
            .await
            .unwrap_err();

        match error {
            WardenError::AgentDefinition(AgentDefinitionError::CustomStepAgentNotFound {
                role,
                expected_path,
            }) => {
                assert_eq!(role, "techlead");
                assert_eq!(
                    expected_path,
                    repo.path().join(CUSTOM_STEP_AGENTS_DIR).join("techlead.md")
                );
            }
            other => panic!("expected CustomStepAgentNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_invalid_custom_step_definition_is_a_typed_error_naming_the_path() {
        let repo = TempDir::new().unwrap();
        tokio::fs::create_dir_all(repo.path().join(CUSTOM_STEP_AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(
            repo.path().join(CUSTOM_STEP_AGENTS_DIR).join("techlead.md"),
            "no frontmatter here\n",
        )
        .await
        .unwrap();

        let error = resolve_custom_step_agent_definition(repo.path(), "techlead", "techlead")
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Invalid { .. })
        ));
    }
}
