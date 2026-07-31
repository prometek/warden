//! Durable quota-continuation wire types (issue #86).
//!
//! Domain types with invariants (`Workflow`, `AgentDefinition`, findings)
//! are represented by plain wire values here and reconstructed through their
//! public parsers/constructors. A persisted row is external input at restore
//! time; direct deserialization must not bypass those validation boundaries.

use serde::{Deserialize, Serialize};

use super::*;

const CHECKPOINT_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunConfigCheckpoint {
    version: u32,
    repo_path: String,
    warden_home: String,
    branch: String,
    intent: String,
    max_review_cycles: u32,
    max_test_cycles: u32,
    workflow: WorkflowCheckpoint,
    max_extra_step_cycles: u32,
    step_agents: Vec<AgentDefinitionCheckpoint>,
    evidence_tool: Option<String>,
    evidence_store_in_repo: bool,
    gate: Option<GateConfigCheckpoint>,
    untrusted_repo_agent_definitions: Vec<UntrustedAgentDefinitionCheckpoint>,
    execution_context: RunExecutionContextCheckpoint,
    quota_anticipation_threshold: f64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowCheckpoint {
    name: String,
    steps: Vec<WorkflowStepCheckpoint>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowStepCheckpoint {
    role: String,
    #[serde(rename = "type")]
    kind: String,
    agent: Option<String>,
    run: Option<String>,
    gate: String,
    budget: Option<String>,
    max_cycles: Option<u32>,
    evidence: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentDefinitionCheckpoint {
    name: Option<String>,
    description: Option<String>,
    tools: Option<String>,
    model: Option<String>,
    system_prompt: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateConfigCheckpoint {
    bare_repo_path: String,
    gated_bin: String,
    repo_slug: Option<String>,
    poll_interval_secs: u64,
    inactivity_timeout_secs: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedAgentDefinitionCheckpoint {
    role: String,
    path: String,
    canonical_path: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunExecutionContextCheckpoint {
    tool: String,
    sandbox: SandboxConfigCheckpoint,
    hooks_toml: Option<String>,
    policy_yaml: Option<String>,
    approval: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SandboxConfigCheckpoint {
    Worktree,
    Docker {
        image: String,
        claude_config_dir: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConvergenceStateCheckpoint {
    version: u32,
    run_base_commit_sha: String,
    base_commit: String,
    cycle_number: u32,
    review_cycle_number: u32,
    test_cycle_number: u32,
    extra_step_cycle_number: u32,
    pending_ci_findings: Vec<FindingCheckpoint>,
    previous_cycle_id: Option<String>,
    step_last_reviewed_commit: Vec<Option<String>>,
    own_step_cycle_numbers: Vec<u32>,
    active_cycle: Option<ActiveCycleCheckpoint>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveCycleCheckpoint {
    cycle_id: String,
    prior_findings: Vec<FindingCheckpoint>,
    producer_base_commit: String,
    phase: ActiveCyclePhaseCheckpoint,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ActiveCyclePhaseCheckpoint {
    Producer,
    Gated {
        producer_result: ProducerResultCheckpoint,
        findings: Vec<FindingCheckpoint>,
        next_step_index: u32,
        entered_extra_budget_this_cycle: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProducerResultCheckpoint {
    commit: String,
    diff: String,
    definition_tampering_finding: Option<FindingCheckpoint>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    let checkpoint =
        RunConfigCheckpoint::from_config(config, execution_context, quota_anticipation_threshold)?;
    serde_json::to_string(&checkpoint)
        .map_err(|source| WardenError::QuotaContinuationEncode { source })
}

pub(super) fn encode_convergence_state(continuation: &ConvergenceContinuation) -> Result<String> {
    serde_json::to_string(&ConvergenceStateCheckpoint::from(continuation))
        .map_err(|source| WardenError::QuotaContinuationEncode { source })
}

pub(super) fn decode_run(run_id: &str, config_json: &str, state_json: &str) -> Result<RestoredRun> {
    let config_checkpoint: RunConfigCheckpoint =
        serde_json::from_str(config_json).map_err(|source| {
            WardenError::QuotaContinuationDecode {
                run_id: run_id.to_string(),
                source,
            }
        })?;
    let state_checkpoint: ConvergenceStateCheckpoint =
        serde_json::from_str(state_json).map_err(|source| {
            WardenError::QuotaContinuationDecode {
                run_id: run_id.to_string(),
                source,
            }
        })?;

    validate_version(run_id, "config", config_checkpoint.version)?;
    validate_version(run_id, "state", state_checkpoint.version)?;

    let quota_anticipation_threshold = config_checkpoint.quota_anticipation_threshold;
    if !(0.0..=1.0).contains(&quota_anticipation_threshold) {
        return Err(invalid(
            run_id,
            format!(
                "quota_anticipation_threshold must be in 0.0..=1.0, got \
                 {quota_anticipation_threshold}"
            ),
        ));
    }

    let (config, execution_context) = config_checkpoint.into_config(run_id)?;
    let continuation = state_checkpoint.into_continuation(run_id, config.workflow.steps.len())?;

    Ok(RestoredRun {
        config,
        execution_context,
        continuation,
        quota_anticipation_threshold,
    })
}

fn validate_version(run_id: &str, section: &str, version: u32) -> Result<()> {
    if version == CHECKPOINT_VERSION {
        return Ok(());
    }
    Err(invalid(
        run_id,
        format!(
            "unsupported {section} checkpoint version {version} (expected {CHECKPOINT_VERSION})"
        ),
    ))
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

impl RunConfigCheckpoint {
    fn from_config(
        config: &RunConfig,
        execution_context: &RunExecutionContext,
        quota_anticipation_threshold: f64,
    ) -> Result<Self> {
        Ok(Self {
            version: CHECKPOINT_VERSION,
            repo_path: exact_path(&config.repo_path, "repo_path")?,
            warden_home: exact_path(&config.warden_home, "warden_home")?,
            branch: config.branch.clone(),
            intent: config.intent.clone(),
            max_review_cycles: config.max_review_cycles,
            max_test_cycles: config.max_test_cycles,
            workflow: WorkflowCheckpoint::from(&config.workflow),
            max_extra_step_cycles: config.max_extra_step_cycles,
            step_agents: config
                .step_agents
                .iter()
                .map(AgentDefinitionCheckpoint::from)
                .collect(),
            evidence_tool: config.evidence_tool.map(|tool| tool.as_str().to_string()),
            evidence_store_in_repo: config.evidence_store_in_repo,
            gate: config
                .gate
                .as_ref()
                .map(GateConfigCheckpoint::from_config)
                .transpose()?,
            untrusted_repo_agent_definitions: config
                .untrusted_repo_agent_definitions
                .iter()
                .map(UntrustedAgentDefinitionCheckpoint::from_config)
                .collect::<Result<Vec<_>>>()?,
            execution_context: RunExecutionContextCheckpoint::from_config(execution_context)?,
            quota_anticipation_threshold,
        })
    }

    fn into_config(self, run_id: &str) -> Result<(RunConfig, RunExecutionContext)> {
        let workflow_json = serde_json::to_string(&self.workflow)
            .map_err(|source| WardenError::QuotaContinuationEncode { source })?;
        let workflow = warden_core::Workflow::parse_yaml(&workflow_json)?;
        let step_agents = self
            .step_agents
            .into_iter()
            .map(AgentDefinitionCheckpoint::into_definition)
            .collect::<warden_core::Result<Vec<_>>>()?;
        let evidence_tool = self
            .evidence_tool
            .as_deref()
            .map(warden_core::EvidenceTool::parse)
            .transpose()?;
        let gate = self.gate.map(GateConfigCheckpoint::into_config);
        let untrusted_repo_agent_definitions = self
            .untrusted_repo_agent_definitions
            .into_iter()
            .map(|entry| entry.into_config(run_id))
            .collect::<Result<Vec<_>>>()?;
        let execution_context = self.execution_context.into_config(run_id)?;

        let config = RunConfig {
            repo_path: PathBuf::from(self.repo_path),
            warden_home: PathBuf::from(self.warden_home),
            branch: self.branch,
            intent: self.intent,
            max_review_cycles: self.max_review_cycles,
            max_test_cycles: self.max_test_cycles,
            workflow,
            max_extra_step_cycles: self.max_extra_step_cycles,
            step_agents,
            evidence_tool,
            evidence_store_in_repo: self.evidence_store_in_repo,
            gate,
            untrusted_repo_agent_definitions,
        };
        Ok((config, execution_context))
    }
}

impl From<&Workflow> for WorkflowCheckpoint {
    fn from(workflow: &Workflow) -> Self {
        Self {
            name: workflow.name.clone(),
            steps: workflow
                .steps
                .iter()
                .map(WorkflowStepCheckpoint::from)
                .collect(),
        }
    }
}

impl From<&WorkflowStep> for WorkflowStepCheckpoint {
    fn from(step: &WorkflowStep) -> Self {
        let (budget, max_cycles) = match step.budget {
            Some(warden_core::StepBudget::Own(max_cycles)) => (None, Some(max_cycles)),
            Some(budget) => (Some(budget.as_str().to_string()), None),
            None => (None, None),
        };
        Self {
            role: step.role.as_str().to_string(),
            kind: step.kind.as_str().to_string(),
            agent: step.agent.clone(),
            run: step.run.clone(),
            gate: step.gate.as_str().to_string(),
            budget,
            max_cycles,
            evidence: step.captures_evidence,
        }
    }
}

impl From<&AgentDefinition> for AgentDefinitionCheckpoint {
    fn from(definition: &AgentDefinition) -> Self {
        Self {
            name: definition.name.clone(),
            description: definition.description.clone(),
            tools: definition.tools.clone(),
            model: definition.model.clone(),
            system_prompt: definition.system_prompt.clone(),
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
            bare_repo_path: PathBuf::from(self.bare_repo_path),
            gated_bin: PathBuf::from(self.gated_bin),
            repo_slug: self.repo_slug,
            poll_interval_secs: self.poll_interval_secs,
            inactivity_timeout_secs: self.inactivity_timeout_secs,
        }
    }
}

impl UntrustedAgentDefinitionCheckpoint {
    fn from_config(config: &UntrustedRepoAgentDefinition) -> Result<Self> {
        Ok(Self {
            role: config.role.as_str().to_string(),
            path: exact_path(&config.path, "untrusted_agent.path")?,
            canonical_path: exact_path(&config.canonical_path, "untrusted_agent.canonical_path")?,
        })
    }

    fn into_config(self, run_id: &str) -> Result<UntrustedRepoAgentDefinition> {
        let role = AgentRole::parse(&self.role).map_err(|error| {
            invalid(
                run_id,
                format!("invalid untrusted agent role {:?}: {error}", self.role),
            )
        })?;
        Ok(UntrustedRepoAgentDefinition {
            role,
            path: PathBuf::from(self.path),
            canonical_path: PathBuf::from(self.canonical_path),
        })
    }
}

impl RunExecutionContextCheckpoint {
    fn from_config(config: &RunExecutionContext) -> Result<Self> {
        let sandbox = match &config.sandbox {
            SandboxConfig::Worktree => SandboxConfigCheckpoint::Worktree,
            SandboxConfig::Docker {
                image,
                claude_config_dir,
            } => SandboxConfigCheckpoint::Docker {
                image: image.clone(),
                claude_config_dir: exact_path(
                    claude_config_dir,
                    "execution_context.sandbox.claude_config_dir",
                )?,
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
            } => SandboxConfig::Docker {
                image,
                claude_config_dir: PathBuf::from(claude_config_dir),
            },
        };
        let approval = match self.approval.as_str() {
            "interactive_tty" => ApprovalConfig::InteractiveTty,
            "fail_closed" => ApprovalConfig::FailClosed,
            other => {
                return Err(invalid(
                    run_id,
                    format!("unknown approval context {other:?}"),
                ));
            }
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

impl From<&ConvergenceContinuation> for ConvergenceStateCheckpoint {
    fn from(continuation: &ConvergenceContinuation) -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            run_base_commit_sha: continuation.run_base_commit_sha.clone(),
            base_commit: continuation.base_commit.clone(),
            cycle_number: continuation.cycle_number,
            review_cycle_number: continuation.review_cycle_number,
            test_cycle_number: continuation.test_cycle_number,
            extra_step_cycle_number: continuation.extra_step_cycle_number,
            pending_ci_findings: continuation
                .pending_ci_findings
                .iter()
                .map(FindingCheckpoint::from)
                .collect(),
            previous_cycle_id: continuation.previous_cycle_id.clone(),
            step_last_reviewed_commit: continuation.step_last_reviewed_commit.clone(),
            own_step_cycle_numbers: continuation.own_step_cycle_numbers.clone(),
            active_cycle: continuation
                .active_cycle
                .as_ref()
                .map(ActiveCycleCheckpoint::from),
        }
    }
}

impl ConvergenceStateCheckpoint {
    fn into_continuation(
        self,
        run_id: &str,
        total_steps: usize,
    ) -> Result<ConvergenceContinuation> {
        if self.cycle_number == 0 {
            return Err(invalid(run_id, "cycle_number must be at least 1"));
        }
        if self.run_base_commit_sha.trim().is_empty() || self.base_commit.trim().is_empty() {
            return Err(invalid(
                run_id,
                "run_base_commit_sha and base_commit must not be blank",
            ));
        }
        if self.step_last_reviewed_commit.len() != total_steps
            || self.own_step_cycle_numbers.len() != total_steps
        {
            return Err(invalid(
                run_id,
                format!(
                    "per-step checkpoint vectors must both contain {total_steps} entries \
                     (reviewed={}, own_cycles={})",
                    self.step_last_reviewed_commit.len(),
                    self.own_step_cycle_numbers.len()
                ),
            ));
        }

        Ok(ConvergenceContinuation {
            run_base_commit_sha: self.run_base_commit_sha,
            base_commit: self.base_commit,
            cycle_number: self.cycle_number,
            review_cycle_number: self.review_cycle_number,
            test_cycle_number: self.test_cycle_number,
            extra_step_cycle_number: self.extra_step_cycle_number,
            pending_ci_findings: findings_from_checkpoints(run_id, self.pending_ci_findings)?,
            previous_cycle_id: self.previous_cycle_id,
            step_last_reviewed_commit: self.step_last_reviewed_commit,
            own_step_cycle_numbers: self.own_step_cycle_numbers,
            active_cycle: self
                .active_cycle
                .map(|cycle| cycle.into_continuation(run_id, total_steps))
                .transpose()?,
        })
    }
}

impl From<&ActiveCycleContinuation> for ActiveCycleCheckpoint {
    fn from(cycle: &ActiveCycleContinuation) -> Self {
        let phase = match &cycle.phase {
            ActiveCyclePhase::Producer => ActiveCyclePhaseCheckpoint::Producer,
            ActiveCyclePhase::Gated {
                producer_result,
                findings,
                next_step_index,
                entered_extra_budget_this_cycle,
            } => ActiveCyclePhaseCheckpoint::Gated {
                producer_result: ProducerResultCheckpoint::from(producer_result),
                findings: findings.iter().map(FindingCheckpoint::from).collect(),
                next_step_index: *next_step_index,
                entered_extra_budget_this_cycle: *entered_extra_budget_this_cycle,
            },
        };
        Self {
            cycle_id: cycle.cycle_id.clone(),
            prior_findings: cycle
                .prior_findings
                .iter()
                .map(FindingCheckpoint::from)
                .collect(),
            producer_base_commit: cycle.producer_base_commit.clone(),
            phase,
        }
    }
}

impl ActiveCycleCheckpoint {
    fn into_continuation(
        self,
        run_id: &str,
        total_steps: usize,
    ) -> Result<ActiveCycleContinuation> {
        if self.cycle_id.trim().is_empty() || self.producer_base_commit.trim().is_empty() {
            return Err(invalid(
                run_id,
                "active cycle id and producer base commit must not be blank",
            ));
        }
        let phase = match self.phase {
            ActiveCyclePhaseCheckpoint::Producer => ActiveCyclePhase::Producer,
            ActiveCyclePhaseCheckpoint::Gated {
                producer_result,
                findings,
                next_step_index,
                entered_extra_budget_this_cycle,
            } => {
                let next_step = usize::try_from(next_step_index).map_err(|_| {
                    invalid(
                        run_id,
                        format!("next_step_index {next_step_index} does not fit usize"),
                    )
                })?;
                if next_step == 0 || next_step >= total_steps {
                    return Err(invalid(
                        run_id,
                        format!(
                            "next_step_index {next_step_index} is outside workflow steps \
                             1..{total_steps}"
                        ),
                    ));
                }
                ActiveCyclePhase::Gated {
                    producer_result: producer_result.into_result(run_id)?,
                    findings: findings_from_checkpoints(run_id, findings)?,
                    next_step_index,
                    entered_extra_budget_this_cycle,
                }
            }
        };
        Ok(ActiveCycleContinuation {
            cycle_id: self.cycle_id,
            prior_findings: findings_from_checkpoints(run_id, self.prior_findings)?,
            producer_base_commit: self.producer_base_commit,
            phase,
        })
    }
}

impl From<&ProducerCycleResult> for ProducerResultCheckpoint {
    fn from(result: &ProducerCycleResult) -> Self {
        Self {
            commit: result.commit.clone(),
            diff: result.diff.clone(),
            definition_tampering_finding: result
                .definition_tampering_finding
                .as_ref()
                .map(FindingCheckpoint::from),
        }
    }
}

impl ProducerResultCheckpoint {
    fn into_result(self, run_id: &str) -> Result<ProducerCycleResult> {
        if self.commit.trim().is_empty() {
            return Err(invalid(run_id, "checkpointed producer commit is blank"));
        }
        Ok(ProducerCycleResult {
            commit: self.commit,
            diff: self.diff,
            definition_tampering_finding: self
                .definition_tampering_finding
                .map(|finding| finding.into_finding(run_id))
                .transpose()?,
        })
    }
}

impl From<&Finding> for FindingCheckpoint {
    fn from(finding: &Finding) -> Self {
        Self {
            source: finding.source.as_str().to_string(),
            severity: finding.severity.as_str().to_string(),
            file: finding.file.clone(),
            description: finding.description.clone(),
            action: finding.action.clone(),
        }
    }
}

impl FindingCheckpoint {
    fn into_finding(self, run_id: &str) -> Result<Finding> {
        let source = warden_core::FindingSource::parse(&self.source).map_err(|error| {
            invalid(
                run_id,
                format!(
                    "invalid checkpointed finding source {:?}: {error}",
                    self.source
                ),
            )
        })?;
        let severity = warden_core::Severity::parse(&self.severity).map_err(|error| {
            invalid(
                run_id,
                format!(
                    "invalid checkpointed finding severity {:?}: {error}",
                    self.severity
                ),
            )
        })?;
        Ok(Finding {
            source,
            severity,
            file: self.file,
            description: self.description,
            action: self.action,
        })
    }
}

fn findings_from_checkpoints(
    run_id: &str,
    findings: Vec<FindingCheckpoint>,
) -> Result<Vec<Finding>> {
    findings
        .into_iter()
        .map(|finding| finding.into_finding(run_id))
        .collect()
}
