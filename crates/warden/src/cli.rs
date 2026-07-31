//! Command-line schema and boundary parsers for the `warden` binary.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use warden::tool_adapter::ToolName;

#[derive(Parser)]
#[command(
    name = "warden",
    version,
    about = "Local orchestrator for AI-assisted convergence loops"
)]
pub(crate) struct Cli {
    /// Increase log verbosity (-v, -vv, -vvv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub(crate) verbose: u8,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Run a full convergence loop against a repository: a producer step
    /// (the coder, by default) followed by a sequence of gated steps
    /// (reviewer/tester by default; customizable via `.warden/workflow.yaml`,
    /// issue #73), reboucling to the producer on a blocking finding. Each
    /// step runs sequentially, in its own worktree synced onto the
    /// producer's commit (ADR-0003).
    Run {
        /// Path to the user's existing repository. Never written to
        /// directly; only worktrees created under `--warden-home` are.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        repo: PathBuf,

        /// Task description passed to producer agent. Repeatable; multiple
        /// values switch to batch mode.
        #[arg(long = "intent", value_parser = parse_intent)]
        intent: Vec<String>,

        /// File containing one intent per non-blank, non-comment line.
        #[arg(long = "intents-file", value_parser = clap::value_parser!(PathBuf))]
        intents_file: Option<PathBuf>,

        /// Stop batch at first non-converged intent.
        #[arg(long)]
        fail_fast: bool,

        /// Branch name recorded for this run.
        #[arg(long, default_value = "main")]
        branch: String,

        /// Maximum coder/reviewer round trips.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
        max_review_cycles: u32,

        /// Maximum tester cycles.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
        max_test_cycles: u32,

        /// Shared cycle budget for extra workflow steps.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
        max_cycles: u32,

        /// Quota-consumption fraction that pauses before next workflow step.
        #[arg(long, default_value_t = 0.90, value_parser = parse_quota_anticipation_threshold)]
        quota_anticipation_threshold: f64,

        /// Warden state directory. Defaults to `~/.warden`.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        warden_home: Option<PathBuf>,

        /// Built-in tool adapter used by every role.
        #[arg(long, value_parser = parse_tool)]
        tool: ToolName,

        /// Allow repository-supplied reviewer/tester definitions.
        #[arg(long)]
        trust_repo_agents: bool,

        /// Override automatic evidence-tool detection.
        #[arg(long, value_parser = parse_evidence_tool)]
        evidence_tool: Option<warden_core::EvidenceTool>,

        /// Commit evidence into `.warden/evidence/<cycle>/`.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        evidence_store_in_repo: bool,

        /// Local bare gate repository.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        gate_bare_repo: Option<PathBuf>,

        /// Installed `warden-gated` binary.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        gate_gated_bin: Option<PathBuf>,

        /// Explicit `owner/repo` provider slug.
        #[arg(long)]
        gate_repo_slug: Option<String>,

        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..))]
        gate_poll_interval_secs: u64,

        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..))]
        gate_inactivity_timeout_secs: u64,

        /// Spawn `warden-tui attach` for this run.
        #[arg(long)]
        tui: bool,

        /// Override binary spawned by `--tui`.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        tui_bin: Option<PathBuf>,

        /// Agent execution backend.
        #[arg(long, default_value = "worktree", value_parser = parse_isolation)]
        isolation: Isolation,

        /// Docker image used by `--isolation docker`.
        #[arg(long, default_value = DEFAULT_DOCKER_IMAGE)]
        isolation_image: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Isolation {
    Worktree,
    Docker,
}

pub(crate) fn parse_isolation(raw: &str) -> Result<Isolation, String> {
    match raw {
        "worktree" => Ok(Isolation::Worktree),
        "docker" => Ok(Isolation::Docker),
        other => Err(format!(
            "unknown --isolation {other:?} (supported: \"worktree\", \"docker\")"
        )),
    }
}

pub(crate) fn isolation_as_str(isolation: Isolation) -> &'static str {
    match isolation {
        Isolation::Worktree => "worktree",
        Isolation::Docker => "docker",
    }
}

const DEFAULT_DOCKER_IMAGE: &str = "warden-agent:latest";

pub(crate) fn parse_tool(raw: &str) -> Result<ToolName, String> {
    ToolName::parse(raw).map_err(|reason| reason.replacen("unknown tool", "unknown --tool", 1))
}

pub(crate) fn parse_quota_anticipation_threshold(raw: &str) -> Result<f64, String> {
    let threshold = raw
        .parse::<f64>()
        .map_err(|_| "quota anticipation threshold must be a number in 0.0..=1.0".to_string())?;
    if threshold.is_finite() && (0.0..=1.0).contains(&threshold) {
        Ok(threshold)
    } else {
        Err("quota anticipation threshold must be a finite number in 0.0..=1.0".to_string())
    }
}

pub(crate) fn tool_as_str(tool: ToolName) -> &'static str {
    tool.as_str()
}

pub(crate) struct IsolationConfig {
    pub(crate) isolation: Isolation,
    pub(crate) image: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TrustRepoAgents(pub(crate) bool);

pub(crate) fn parse_evidence_tool(raw: &str) -> Result<warden_core::EvidenceTool, String> {
    warden_core::EvidenceTool::parse(raw).map_err(|error| error.to_string())
}

pub(crate) fn parse_intent(raw: &str) -> Result<String, String> {
    if raw.trim().is_empty() {
        return Err("run intent must not be blank".to_string());
    }
    Ok(raw.to_string())
}
