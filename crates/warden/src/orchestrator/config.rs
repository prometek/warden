//! Static run configuration types for the convergence loop -- resolved once
//! by the CLI (`main.rs`) and handed to [`super::Orchestrator::run_convergence_loop`].

use std::path::PathBuf;

use warden_core::{AgentDefinition, AgentRole, EvidenceTool};

/// Static configuration for a single run of the convergence loop.
///
/// `coder_agent`/`reviewer_agent`/`tester_agent` are the markdown
/// definitions of each role (issue #24): what the role *is* (its system
/// prompt, and optionally its `tools`/`model`). Resolved once, from the
/// run's base repo, before this run's `runs` row is even written --
/// `warden::agent_def::resolve_agent_definition`'s own docs explain why
/// that resolve-once-at-the-base-repo timing matters for reviewer/tester
/// independence. A [`ToolAdapter`] maps each onto the concrete CLI to spawn
/// -- ADR-0005: Warden spawns whatever CLI a `--tool` adapter builds, never
/// calls an LLM API directly, and hardcodes no agent binary of its own (the
/// adapter is what knows the binary name).
pub struct RunConfig {
    /// The user's pre-existing repository. Never written to directly — only
    /// read to resolve the starting commit and to run `git worktree`.
    pub repo_path: PathBuf,
    /// Root directory for Warden's own state (`<warden_home>/worktrees/...`).
    pub warden_home: PathBuf,
    pub branch: String,
    pub intent: String,
    /// Issue #43/ADR-0014: the coder<->reviewer round-trip budget
    /// (`RunState::Reviewing`) -- a scoped re-review's own finding is charged
    /// here even when a tester finding was what triggered the coder's
    /// correctif (decision #37 Q1).
    pub max_review_cycles: u32,
    /// Issue #43/ADR-0014: how many times the tester may actually run and
    /// come back with a blocking finding (`RunState::Testing`) before the run
    /// gives up as `MaxTestCyclesExceeded`.
    pub max_test_cycles: u32,
    pub coder_agent: AgentDefinition,
    pub reviewer_agent: AgentDefinition,
    pub tester_agent: AgentDefinition,
    /// Overrides automatic project-type detection for the Evidence Capture
    /// Adapter (`evidence.tool`, ADR-0009). `None` means "detect from the
    /// repo" (`warden_core::detect_project_type`).
    pub evidence_tool: Option<EvidenceTool>,
    /// Whether captured evidence gets committed into `.warden/evidence/` and
    /// pushed with the converged commit (`evidence.store_in_repo`,
    /// ADR-0009). Defaults to `true` at the CLI layer -- kept required here
    /// rather than defaulted in this struct so every caller states its
    /// choice explicitly.
    pub evidence_store_in_repo: bool,
    /// Issue #15/ADR-0011's post-`Converged` tail (push into the local bare
    /// gate repo + PR open/finalize + CI watch). `None` preserves this
    /// crate's original behaviour exactly: a converged run stops at
    /// `Converged` and never reaches `Pushed`/`AwaitingCi`/`Done`.
    pub gate: Option<GateConfig>,
    /// Issue #26: the reviewer/tester definitions (if any) that were
    /// actually resolved from the repo under review rather than the user
    /// config directory -- only ever non-empty when `--trust-repo-agents`
    /// was passed *and* no user-config file existed for that role (see
    /// `agent_def::resolve_agent_definition`'s own docs on precedence).
    /// Published as [`warden_core::RunEvent::UntrustedAgentDefinitionUsed`]
    /// right after `RunStarted` (see [`Orchestrator::run_convergence_loop`])
    /// so this run's own event log carries a permanent, replayable record of
    /// which role(s) ran under a definition the coder can write to --
    /// `main.rs`'s `tracing::warn!` at resolution time is not itself
    /// persisted anywhere a later `warden-tui attach`/history query could
    /// still see it.
    pub untrusted_repo_agent_definitions: Vec<UntrustedRepoAgentDefinition>,
}

/// One reviewer/tester definition sourced from the repo under review under
/// `--trust-repo-agents` (issue #26) -- see the
/// [`RunConfig::untrusted_repo_agent_definitions`] field docs.
#[derive(Debug, Clone)]
pub struct UntrustedRepoAgentDefinition {
    pub role: AgentRole,
    /// The literal, pre-canonicalization path that was actually read.
    pub path: PathBuf,
    /// Issue #26 review, LOW: what `path` actually canonicalizes to
    /// (symlinks resolved) -- see
    /// [`warden_core::RunEvent::UntrustedAgentDefinitionUsed`]'s own docs for
    /// why this is carried alongside `path`, never instead of it.
    pub canonical_path: PathBuf,
}

/// Everything [`Orchestrator::drive_post_convergence_tail`] needs to trigger
/// `warden-gated`'s side of issue #15/ADR-0011's tail. Deliberately plain
/// data (no trigger trait object): the concrete
/// [`crate::gate_trigger::SubprocessGateTrigger`] used in production is
/// built from these fields at the one call site that needs it, and
/// `drive_post_convergence_tail` itself stays generic over
/// [`crate::gate_trigger::GateTrigger`] so tests can substitute a fake.
#[derive(Debug, Clone)]
pub struct GateConfig {
    /// The local bare gate repo `warden` pushes the converged commit into
    /// (ADR-0002) -- the same repo `warden-gated run-tail`/`resume-watch`
    /// push the PR's content from.
    pub bare_repo_path: PathBuf,
    /// Absolute path to the installed `warden-gated` binary.
    pub gated_bin: PathBuf,
    /// Explicit `owner/repo` override; `None` lets `warden-gated` resolve it
    /// from the bare repo's `origin` remote (`GhProvider::new`).
    pub repo_slug: Option<String>,
    pub poll_interval_secs: u64,
    pub inactivity_timeout_secs: u64,
}
