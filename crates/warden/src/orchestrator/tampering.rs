use super::*;

pub(super) struct AgentDefinitionSnapshot {
    definitions: std::collections::BTreeMap<String, agent_def::RawDefinition>,
}

pub(super) const SNAPSHOT_WORKTREE_ROLE: &str = "agent-definition-snapshot";

const TAMPERING_CHECK_WORKTREE_ROLE: &str = "agent-definition-check";

impl AgentDefinitionSnapshot {
    pub(super) async fn capture(
        worktree_manager: &WorktreeManager,
        run_id: &str,
        label: &str,
        commit_ish: &str,
        agent_names: &[String],
    ) -> Result<Self> {
        let worktree = worktree_manager.create(run_id, label, commit_ish).await?;
        let mut definitions = std::collections::BTreeMap::new();
        for name in agent_names {
            definitions.insert(
                name.clone(),
                agent_def::read_raw_definition(worktree.path(), name).await,
            );
        }
        let snapshot = Self { definitions };

        worktree.remove().await?;
        Ok(snapshot)
    }
}

/// (cross-run agent-definition poisoning).
pub(super) async fn agent_definition_tampering_finding(
    worktree_manager: &WorktreeManager,
    run_id: &str,
    new_commit: &str,
    run_start_snapshot: &AgentDefinitionSnapshot,
) -> Result<Option<Finding>> {
    let resolved_now = AgentDefinitionSnapshot::capture(
        worktree_manager,
        run_id,
        TAMPERING_CHECK_WORKTREE_ROLE,
        new_commit,
        &run_start_snapshot
            .definitions
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
    )
    .await?;

    let mut diverged_paths = Vec::new();
    let mut unreadable_details = Vec::new();
    for (name, original) in &run_start_snapshot.definitions {
        let now = &resolved_now.definitions[name];
        if now != original {
            let path = format!("{}/{name}.md", agent_def::AGENTS_DIR);
            if let agent_def::RawDefinition::Unreadable { message, .. } = now {
                unreadable_details.push(format!("{path} ({message})"));
            }
            diverged_paths.push(path);
        }
    }

    if diverged_paths.is_empty() {
        return Ok(None);
    }

    let unreadable_suffix = if unreadable_details.is_empty() {
        String::new()
    } else {
        format!(" -- now unreadable: {}", unreadable_details.join("; "))
    };

    Ok(Some(Finding {
        source: warden_core::FindingSource::Warden,
        severity: warden_core::Severity::Blocking,
        file: diverged_paths.first().cloned(),
        description: format!(
            "this step's commit changes agent definitions resolved at run start: {}. A future \
             run would receive different prompts or tool grants; human review is required{}",
            diverged_paths.join(", "),
            unreadable_suffix,
        ),
        action: Some(format!(
            "review changes to {}; revert them if they are not intentional",
            diverged_paths.join(", "),
        )),
    }))
}
