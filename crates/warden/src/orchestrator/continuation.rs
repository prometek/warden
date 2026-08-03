use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::*;

const CHECKPOINT_VERSION: u32 = 4;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunConfigCheckpoint {
    version: u32,
    repo_path: String,
    warden_home: String,
    branch: String,
    intent: String,
    max_cycles: u32,
    workflow: WorkflowCheckpoint,
    step_agents: Vec<AgentDefinitionCheckpoint>,
    repository_agent_definitions: bool,
    evidence_tool: Option<String>,
    evidence_store_in_repo: bool,
    gate: Option<GateConfigCheckpoint>,
    execution_context: RunExecutionContextCheckpoint,
    quota_anticipation_threshold: f64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowCheckpoint {
    name: String,
    entry: String,
    steps: BTreeMap<String, WorkflowStepCheckpoint>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowStepCheckpoint {
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    on_clean: String,
    on_blocking: String,
    on_error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_cycles: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    evidence: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct AgentDefinitionCheckpoint {
    name: Option<String>,
    description: Option<String>,
    tools: Option<String>,
    model: Option<String>,
    system_prompt: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GateConfigCheckpoint {
    bare_repo_path: String,
    gated_bin: String,
    repo_slug: Option<String>,
    poll_interval_secs: u64,
    inactivity_timeout_secs: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct RunExecutionContextCheckpoint {
    tool: String,
    sandbox: SandboxConfigCheckpoint,
    hooks_toml: Option<String>,
    policy_yaml: Option<String>,
    approval: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SandboxConfigCheckpoint {
    Worktree,
    Docker {
        image: String,
        claude_config_dir: String,
        cpus: Option<String>,
        memory: Option<String>,
        network: Option<String>,
        egress_proxy: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct ConvergenceStateCheckpoint {
    version: u32,
    run_base_commit_sha: String,
    base_commit: String,
    cycle_number: u32,
    step_cycle_numbers: Vec<u32>,
    pending_ci_findings: Vec<FindingCheckpoint>,
    previous_cycle_id: Option<String>,
    next_step_index: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct FindingCheckpoint {
    source: String,
    severity: String,
    file: Option<String>,
    description: String,
    action: Option<String>,
}

pub(super) struct RestoredRun {
    pub config: RunConfig,
    pub execution_context: RunExecutionContext,
    pub continuation: ConvergenceContinuation,
    pub quota_anticipation_threshold: f64,
}

pub(super) fn encode_run_config(
    config: &RunConfig,
    execution_context: &RunExecutionContext,
    quota_anticipation_threshold: f64,
) -> Result<String> {
    let checkpoint = RunConfigCheckpoint {
        version: CHECKPOINT_VERSION,
        repo_path: exact_path(&config.repo_path, "repo_path")?,
        warden_home: exact_path(&config.warden_home, "warden_home")?,
        branch: config.branch.clone(),
        intent: config.intent.clone(),
        max_cycles: config.max_cycles,
        workflow: WorkflowCheckpoint::from(&config.workflow),
        step_agents: config
            .step_agents
            .iter()
            .map(AgentDefinitionCheckpoint::from)
            .collect(),
        repository_agent_definitions: config.repository_agent_definitions,
        evidence_tool: config.evidence_tool.map(|tool| tool.as_str().to_string()),
        evidence_store_in_repo: config.evidence_store_in_repo,
        gate: config
            .gate
            .as_ref()
            .map(GateConfigCheckpoint::from_config)
            .transpose()?,
        execution_context: RunExecutionContextCheckpoint::from_config(execution_context)?,
        quota_anticipation_threshold,
    };
    serde_json::to_string(&checkpoint)
        .map_err(|source| WardenError::QuotaContinuationEncode { source })
}

pub(super) fn encode_convergence_state(continuation: &ConvergenceContinuation) -> Result<String> {
    let checkpoint = ConvergenceStateCheckpoint {
        version: CHECKPOINT_VERSION,
        run_base_commit_sha: continuation.run_base_commit_sha.clone(),
        base_commit: continuation.base_commit.clone(),
        cycle_number: continuation.cycle_number,
        step_cycle_numbers: continuation.step_cycle_numbers.clone(),
        pending_ci_findings: continuation
            .pending_ci_findings
            .iter()
            .map(FindingCheckpoint::from)
            .collect(),
        previous_cycle_id: continuation.previous_cycle_id.clone(),
        next_step_index: continuation.next_step_index,
    };
    serde_json::to_string(&checkpoint)
        .map_err(|source| WardenError::QuotaContinuationEncode { source })
}

pub(super) fn decode_run(run_id: &str, config_json: &str, state_json: &str) -> Result<RestoredRun> {
    let config: RunConfigCheckpoint = decode(run_id, config_json)?;
    let state: ConvergenceStateCheckpoint = decode(run_id, state_json)?;
    validate_version(run_id, config.version)?;
    validate_version(run_id, state.version)?;
    if !(0.0..=1.0).contains(&config.quota_anticipation_threshold) {
        return Err(invalid(
            run_id,
            "quota anticipation threshold is outside 0..=1",
        ));
    }

    let workflow_raw = serde_json::to_string(&config.workflow).map_err(|source| {
        invalid(
            run_id,
            format!("cannot decode workflow checkpoint: {source}"),
        )
    })?;
    let workflow = Workflow::parse_yaml(&workflow_raw)?;
    if state.cycle_number == 0
        || state.run_base_commit_sha.trim().is_empty()
        || state.base_commit.trim().is_empty()
        || state.step_cycle_numbers.len() != workflow.steps.len()
        || state.next_step_index as usize >= workflow.steps.len()
    {
        return Err(invalid(run_id, "invalid generic convergence checkpoint"));
    }
    let step_agents = config
        .step_agents
        .into_iter()
        .map(AgentDefinitionCheckpoint::into_definition)
        .collect::<warden_core::Result<Vec<_>>>()?;
    let execution_context = config.execution_context.into_config(run_id)?;
    let continuation = ConvergenceContinuation {
        run_base_commit_sha: state.run_base_commit_sha,
        base_commit: state.base_commit,
        cycle_number: state.cycle_number,
        step_cycle_numbers: state.step_cycle_numbers,
        pending_ci_findings: findings_from_checkpoints(run_id, state.pending_ci_findings)?,
        previous_cycle_id: state.previous_cycle_id,
        next_step_index: state.next_step_index,
    };
    let run_config = RunConfig {
        repo_path: PathBuf::from(config.repo_path),
        warden_home: PathBuf::from(config.warden_home),
        branch: config.branch,
        intent: config.intent,
        max_cycles: config.max_cycles,
        workflow,
        step_agents,
        repository_agent_definitions: config.repository_agent_definitions,
        evidence_tool: config
            .evidence_tool
            .as_deref()
            .map(warden_core::EvidenceTool::parse)
            .transpose()?,
        evidence_store_in_repo: config.evidence_store_in_repo,
        gate: config.gate.map(GateConfigCheckpoint::into_config),
    };
    Ok(RestoredRun {
        config: run_config,
        execution_context,
        continuation,
        quota_anticipation_threshold: config.quota_anticipation_threshold,
    })
}

fn decode<T: serde::de::DeserializeOwned>(run_id: &str, raw: &str) -> Result<T> {
    serde_json::from_str(raw).map_err(|source| WardenError::QuotaContinuationDecode {
        run_id: run_id.to_string(),
        source,
    })
}

fn validate_version(run_id: &str, version: u32) -> Result<()> {
    if version == CHECKPOINT_VERSION {
        Ok(())
    } else {
        Err(invalid(
            run_id,
            format!("unsupported checkpoint version {version} (expected {CHECKPOINT_VERSION})"),
        ))
    }
}

fn invalid(run_id: &str, reason: impl Into<String>) -> WardenError {
    WardenError::InvalidQuotaContinuation {
        run_id: run_id.to_string(),
        reason: reason.into(),
    }
}

fn exact_path(path: &Path, field: &'static str) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| WardenError::NonUtf8QuotaContinuationPath {
            field,
            path: path.to_path_buf(),
        })
}

impl From<&Workflow> for WorkflowCheckpoint {
    fn from(workflow: &Workflow) -> Self {
        let target = |target: warden_core::StepTarget| match target {
            warden_core::StepTarget::Step(index) => {
                workflow.steps[index as usize].role.as_str().to_string()
            }
            warden_core::StepTarget::Converged => "converged".to_string(),
            warden_core::StepTarget::Failed => "failed".to_string(),
        };
        Self {
            name: workflow.name.clone(),
            entry: workflow.steps[workflow.entry() as usize]
                .role
                .as_str()
                .to_string(),
            steps: workflow
                .steps
                .iter()
                .map(|step| {
                    (
                        step.role.as_str().to_string(),
                        WorkflowStepCheckpoint {
                            kind: step.kind.as_str().to_string(),
                            agent: step.agent.clone(),
                            run: step.run.clone(),
                            on_clean: target(step.transitions.clean),
                            on_blocking: target(step.transitions.blocking),
                            on_error: target(step.transitions.error),
                            max_cycles: step.max_cycles,
                            evidence: step.captures_evidence,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl From<&AgentDefinition> for AgentDefinitionCheckpoint {
    fn from(value: &AgentDefinition) -> Self {
        Self {
            name: value.name.clone(),
            description: value.description.clone(),
            tools: value.tools.clone(),
            model: value.model.clone(),
            system_prompt: value.system_prompt.clone(),
        }
    }
}

impl AgentDefinitionCheckpoint {
    fn into_definition(self) -> warden_core::Result<AgentDefinition> {
        AgentDefinition::new(
            self.name,
            self.description,
            self.tools,
            self.model,
            self.system_prompt,
        )
    }
}

impl GateConfigCheckpoint {
    fn from_config(config: &GateConfig) -> Result<Self> {
        Ok(Self {
            bare_repo_path: exact_path(&config.bare_repo_path, "gate.bare_repo_path")?,
            gated_bin: exact_path(&config.gated_bin, "gate.gated_bin")?,
            repo_slug: config.repo_slug.clone(),
            poll_interval_secs: config.poll_interval_secs,
            inactivity_timeout_secs: config.inactivity_timeout_secs,
        })
    }

    fn into_config(self) -> GateConfig {
        GateConfig {
            bare_repo_path: self.bare_repo_path.into(),
            gated_bin: self.gated_bin.into(),
            repo_slug: self.repo_slug,
            poll_interval_secs: self.poll_interval_secs,
            inactivity_timeout_secs: self.inactivity_timeout_secs,
        }
    }
}

impl RunExecutionContextCheckpoint {
    fn from_config(config: &RunExecutionContext) -> Result<Self> {
        let sandbox = match &config.sandbox {
            SandboxConfig::Worktree => SandboxConfigCheckpoint::Worktree,
            SandboxConfig::Docker {
                image,
                claude_config_dir,
                run_options,
            } => SandboxConfigCheckpoint::Docker {
                image: image.clone(),
                claude_config_dir: exact_path(claude_config_dir, "docker.claude_config_dir")?,
                cpus: run_options.cpus.clone(),
                memory: run_options.memory.clone(),
                network: run_options
                    .egress
                    .as_ref()
                    .map(|egress| egress.network.clone()),
                egress_proxy: run_options
                    .egress
                    .as_ref()
                    .map(|egress| egress.proxy.clone()),
            },
        };
        Ok(Self {
            tool: config.tool.as_str().to_string(),
            sandbox,
            hooks_toml: config.hooks_toml.clone(),
            policy_yaml: config.policy_yaml.clone(),
            approval: match config.approval {
                ApprovalConfig::InteractiveTty => "interactive_tty",
                ApprovalConfig::FailClosed => "fail_closed",
            }
            .to_string(),
        })
    }

    fn into_config(self, run_id: &str) -> Result<RunExecutionContext> {
        let tool = crate::tool_adapter::ToolName::parse(&self.tool)
            .map_err(|reason| invalid(run_id, reason))?;
        let sandbox = match self.sandbox {
            SandboxConfigCheckpoint::Worktree => SandboxConfig::Worktree,
            SandboxConfigCheckpoint::Docker {
                image,
                claude_config_dir,
                cpus,
                memory,
                network,
                egress_proxy,
            } => SandboxConfig::Docker {
                image,
                claude_config_dir: claude_config_dir.into(),
                run_options: warden_sandbox::DockerRunOptions {
                    cpus,
                    memory,
                    egress: match (network, egress_proxy) {
                        (None, None) => None,
                        (Some(network), Some(proxy)) => {
                            Some(warden_sandbox::DockerEgressConfig { network, proxy })
                        }
                        _ => return Err(invalid(run_id, "incomplete Docker egress config")),
                    },
                },
            },
        };
        let approval = match self.approval.as_str() {
            "interactive_tty" => ApprovalConfig::InteractiveTty,
            "fail_closed" => ApprovalConfig::FailClosed,
            other => return Err(invalid(run_id, format!("unknown approval {other:?}"))),
        };
        Ok(RunExecutionContext {
            tool,
            sandbox,
            hooks_toml: self.hooks_toml,
            policy_yaml: self.policy_yaml,
            approval,
        })
    }
}

impl From<&Finding> for FindingCheckpoint {
    fn from(value: &Finding) -> Self {
        Self {
            source: value.source.as_str().to_string(),
            severity: value.severity.as_str().to_string(),
            file: value.file.clone(),
            description: value.description.clone(),
            action: value.action.clone(),
        }
    }
}

fn findings_from_checkpoints(
    run_id: &str,
    findings: Vec<FindingCheckpoint>,
) -> Result<Vec<Finding>> {
    findings
        .into_iter()
        .map(|finding| {
            Ok(Finding {
                source: warden_core::FindingSource::parse(&finding.source)
                    .map_err(|error| invalid(run_id, error.to_string()))?,
                severity: warden_core::Severity::parse(&finding.severity)
                    .map_err(|error| invalid(run_id, error.to_string()))?,
                file: finding.file,
                description: finding.description,
                action: finding.action,
            })
        })
        .collect()
}
