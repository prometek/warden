//! Static run configuration types for the convergence loop -- resolved once by the CLI (`main.rs`)
//! and handed to [`super::Orchestrator::run_convergence_loop`].

use std::path::PathBuf;
use std::sync::Arc;

use warden_core::{AgentDefinition, AgentRole, EvidenceTool, Workflow};
use warden_sandbox::{DockerConfig, DockerRunOptions, DockerSandbox, LocalSandbox, Sandbox};

use crate::tool_adapter::ToolName;

/// Durable execution and security inputs required to resume a run without consulting a later CLI
/// invocation or mutable repository configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunExecutionContext {
    pub tool: ToolName,
    pub sandbox: SandboxConfig,
    /// Exact `.warden/hooks.toml` contents resolved for the original run.
    pub hooks_toml: Option<String>,
    /// Exact `.warden/policy.yaml` contents resolved for the original run.
    pub policy_yaml: Option<String>,
    pub approval: ApprovalConfig,
}

/// Agent-isolation backend selected for the original run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxConfig {
    Worktree,
    Docker {
        image: String,
        claude_config_dir: PathBuf,
        run_options: DockerRunOptions,
    },
}

impl SandboxConfig {
    pub fn build(&self, repo_path: &std::path::Path) -> Arc<dyn Sandbox> {
        match self {
            Self::Worktree => Arc::new(LocalSandbox::new()),
            Self::Docker {
                image,
                claude_config_dir,
                run_options,
            } => Arc::new(DockerSandbox::new(DockerConfig {
                image: image.clone(),
                repo_path: repo_path.to_path_buf(),
                claude_config_dir: claude_config_dir.clone(),
                run_options: run_options.clone(),
            })),
        }
    }
}

/// Approval channel used by the original process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalConfig {
    InteractiveTty,
    FailClosed,
}

/// Static configuration for a single run of the convergence loop.
pub struct RunConfig {
    /// The user's pre-existing repository.
    pub repo_path: PathBuf,
    /// Root directory for Warden's own state (`<warden_home>/worktrees/...`).
    pub warden_home: PathBuf,
    pub branch: String,
    pub intent: String,
    pub max_review_cycles: u32,
    /// / how many times the tester may actually run and come back with a blocking finding
    /// (`RunState::RunningStep(2)`) before the run gives up as `RunState::StepCyclesExceeded(2)`.
    pub max_test_cycles: u32,
    /// the run's pipeline -- `Workflow::builtin_default()` when no `.warden/workflow.yaml` exists,
    /// or the parsed/validated contents of that file otherwise.
    pub workflow: Workflow,
    pub max_extra_step_cycles: u32,
    /// (trio-unification follow-up): one resolved [`AgentDefinition`] per `workflow.steps`, in the
    /// exact same order.
    pub step_agents: Vec<AgentDefinition>,
    /// Overrides automatic project-type detection for the Evidence Capture Adapter.
    pub evidence_tool: Option<EvidenceTool>,
    /// Whether captured evidence gets committed into `.warden/evidence/` and pushed with the
    /// converged commit.
    pub evidence_store_in_repo: bool,
    /// /'s post-`Converged` tail (push into the local bare gate repo + PR open/finalize + CI
    /// watch).
    pub gate: Option<GateConfig>,
    pub untrusted_repo_agent_definitions: Vec<UntrustedRepoAgentDefinition>,
}

#[derive(Debug, Clone)]
pub struct UntrustedRepoAgentDefinition {
    pub role: AgentRole,
    /// The literal, pre-canonicalization path that was actually read.
    pub path: PathBuf,
    pub canonical_path: PathBuf,
}

/// Configuration for post-convergence gate processing.
#[derive(Debug, Clone)]
pub struct GateConfig {
    /// The local bare gate repo `warden` pushes the converged commit into -- the same repo `warden-
    /// gated run-tail`/`resume-watch` push the PR's content from.
    pub bare_repo_path: PathBuf,
    /// Absolute path to the installed `warden-gated` binary.
    pub gated_bin: PathBuf,
    /// Explicit `owner/repo` override; `None` lets `warden-gated` resolve it from the bare repo's
    /// `origin` remote (`GhProvider::new`).
    pub repo_slug: Option<String>,
    pub poll_interval_secs: u64,
    pub inactivity_timeout_secs: u64,
}
