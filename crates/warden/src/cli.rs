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
    /// Run the repository-defined workflow graph until it converges or fails.
    Run {
        /// Path to the user's existing repository.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        repo: PathBuf,

        /// Task description passed to every agent step.
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

        /// Global cycle budget; a step may declare a lower `max_cycles`.
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

        /// Optional CPU quota for each Docker agent container.
        #[arg(long, value_parser = parse_docker_cpus)]
        docker_cpus: Option<String>,

        /// Optional memory limit for each Docker agent container.
        #[arg(long, value_parser = parse_docker_memory)]
        docker_memory: Option<String>,

        /// Internal Docker network containing the configured egress proxy.
        #[arg(long, requires = "docker_egress_proxy", value_parser = parse_docker_network)]
        docker_network: Option<String>,

        /// HTTP(S) proxy reachable through `--docker-network`.
        #[arg(long, requires = "docker_network", value_parser = parse_docker_egress_proxy)]
        docker_egress_proxy: Option<String>,
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

const DEFAULT_DOCKER_IMAGE: &str = concat!("warden-agent:", env!("CARGO_PKG_VERSION"));

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
    pub(crate) cpus: Option<String>,
    pub(crate) memory: Option<String>,
    pub(crate) network: Option<String>,
    pub(crate) egress_proxy: Option<String>,
}

impl IsolationConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let has_docker_options = self.cpus.is_some()
            || self.memory.is_some()
            || self.network.is_some()
            || self.egress_proxy.is_some();
        if self.isolation == Isolation::Worktree && has_docker_options {
            return Err("--docker-* options require --isolation docker".to_string());
        }
        if self.network.is_some() != self.egress_proxy.is_some() {
            return Err(
                "Docker egress requires both --docker-network and --docker-egress-proxy"
                    .to_string(),
            );
        }
        Ok(())
    }
}

pub(crate) fn parse_docker_cpus(raw: &str) -> Result<String, String> {
    let cpus = raw
        .parse::<f64>()
        .map_err(|_| "Docker CPU limit must be a positive number".to_string())?;
    if cpus.is_finite() && cpus > 0.0 {
        Ok(raw.to_string())
    } else {
        Err("Docker CPU limit must be a finite positive number".to_string())
    }
}

pub(crate) fn parse_docker_memory(raw: &str) -> Result<String, String> {
    let digit_count = raw.bytes().take_while(u8::is_ascii_digit).count();
    let (amount, unit) = raw.split_at(digit_count);
    let valid_amount = amount.parse::<u64>().is_ok_and(|value| value > 0);
    let unit = unit.to_ascii_lowercase();
    if valid_amount && matches!(unit.as_str(), "b" | "k" | "m" | "g" | "kb" | "mb" | "gb") {
        Ok(format!("{amount}{unit}"))
    } else {
        Err("Docker memory limit must be a positive integer followed by b, k, m, or g".to_string())
    }
}

pub(crate) fn parse_docker_network(raw: &str) -> Result<String, String> {
    if !raw.is_empty()
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        Ok(raw.to_string())
    } else {
        Err("Docker network must contain only letters, digits, '.', '_', or '-'".to_string())
    }
}

pub(crate) fn parse_docker_egress_proxy(raw: &str) -> Result<String, String> {
    let authority = raw
        .strip_prefix("http://")
        .or_else(|| raw.strip_prefix("https://"))
        .and_then(|rest| rest.split('/').next());
    if !raw.chars().any(char::is_whitespace)
        && authority.is_some_and(|value| !value.is_empty() && !value.contains('@'))
    {
        Ok(raw.to_string())
    } else {
        Err("Docker egress proxy must be an HTTP(S) URL without embedded credentials".to_string())
    }
}

pub(crate) fn parse_evidence_tool(raw: &str) -> Result<warden_core::EvidenceTool, String> {
    warden_core::EvidenceTool::parse(raw).map_err(|error| error.to_string())
}

pub(crate) fn parse_intent(raw: &str) -> Result<String, String> {
    if raw.trim().is_empty() {
        return Err("run intent must not be blank".to_string());
    }
    Ok(raw.to_string())
}
