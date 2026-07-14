//! `warden` binary: CLI parsing + dispatch only. All orchestration logic
//! lives in the `warden` library crate (`src/lib.rs` and friends).

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use warden::db;
use warden::orchestrator::{self, Orchestrator, RunConfig};
use warden::process::AgentCommand;

#[derive(Parser)]
#[command(
    name = "warden",
    version,
    about = "Local orchestrator for AI-assisted convergence loops"
)]
struct Cli {
    /// Increase log verbosity (-v, -vv, -vvv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a full convergence loop (coder -> [review ∥ test] -> reboucle if
    /// needed) against a repository. Reviewer and tester run in parallel,
    /// each in its own worktree synced onto the coder's commit (ADR-0003).
    Run {
        /// Path to the user's existing repository. Never written to
        /// directly; only worktrees created under `--warden-home` are.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        repo: PathBuf,

        /// The task description passed to the coder agent.
        #[arg(long)]
        intent: String,

        /// Branch name recorded for this run (informational in Phase 1;
        /// no push happens until the git gate lands in Phase 3).
        #[arg(long, default_value = "main")]
        branch: String,

        /// Maximum number of coder/review/test cycles before giving up
        /// (`RunState::MaxCyclesExceeded`). Must be at least 1 — a budget
        /// of 0 could never let the coder run at all.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
        max_cycles: u32,

        /// Warden's own state directory (SQLite db + worktrees). Defaults
        /// to `~/.warden`.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        warden_home: Option<PathBuf>,

        /// Coder agent command, e.g. `--coder-cmd "claude -p coder.md"`.
        #[arg(long)]
        coder_cmd: String,

        /// Reviewer agent command; stdout must be the findings JSON
        /// protocol described in `warden_core::parse_findings`.
        #[arg(long)]
        reviewer_cmd: String,

        /// Tester agent command; same findings JSON protocol as reviewer.
        #[arg(long)]
        tester_cmd: String,

        /// Overrides automatic project-type detection for the Evidence
        /// Capture Adapter (ADR-0009): `playwright` for web/UI projects,
        /// `asciinema` for CLI projects. Detected from the repo when
        /// omitted.
        #[arg(long, value_parser = parse_evidence_tool)]
        evidence_tool: Option<warden_core::EvidenceTool>,

        /// Commits captured evidence into `.warden/evidence/<cycle>/` so it
        /// lands in the finalized PR (ADR-0009). Enabled by default.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        evidence_store_in_repo: bool,

        /// Issue #15/ADR-0011: the local bare gate repo to push a converged
        /// run's tail into. Omitted means the post-Converged tail (push +
        /// PR open/finalize + CI watch) is skipped entirely -- a converged
        /// run stops at `Converged`, exactly like before this issue.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        gate_bare_repo: Option<PathBuf>,

        /// Absolute path to the installed `warden-gated` binary -- required
        /// alongside `--gate-bare-repo` to spawn `run-tail`/`resume-watch`.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        gate_gated_bin: Option<PathBuf>,

        /// Explicit `owner/repo` override for the PR provider, bypassing
        /// `origin` remote detection.
        #[arg(long)]
        gate_repo_slug: Option<String>,

        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..))]
        gate_poll_interval_secs: u64,

        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..))]
        gate_inactivity_timeout_secs: u64,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Commands::Run {
            repo,
            intent,
            branch,
            max_cycles,
            warden_home,
            coder_cmd,
            reviewer_cmd,
            tester_cmd,
            evidence_tool,
            evidence_store_in_repo,
            gate_bare_repo,
            gate_gated_bin,
            gate_repo_slug,
            gate_poll_interval_secs,
            gate_inactivity_timeout_secs,
        } => {
            // Issue #15/ADR-0011: the post-Converged tail only runs when
            // both paths it needs are configured; omitting either preserves
            // this crate's original behaviour (stop at `Converged`).
            let gate = match (gate_bare_repo, gate_gated_bin) {
                (Some(bare_repo_path), Some(gated_bin)) => Some(orchestrator::GateConfig {
                    bare_repo_path,
                    gated_bin,
                    repo_slug: gate_repo_slug,
                    poll_interval_secs: gate_poll_interval_secs,
                    inactivity_timeout_secs: gate_inactivity_timeout_secs,
                }),
                _ => None,
            };

            run(
                repo,
                intent,
                branch,
                max_cycles,
                warden_home,
                coder_cmd,
                reviewer_cmd,
                tester_cmd,
                evidence_tool,
                evidence_store_in_repo,
                gate,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    repo: PathBuf,
    intent: String,
    branch: String,
    max_cycles: u32,
    warden_home: Option<PathBuf>,
    coder_cmd: String,
    reviewer_cmd: String,
    tester_cmd: String,
    evidence_tool: Option<warden_core::EvidenceTool>,
    evidence_store_in_repo: bool,
    gate: Option<orchestrator::GateConfig>,
) -> anyhow::Result<()> {
    let warden_home = warden_home.unwrap_or(default_warden_home()?);
    let db_path = warden_home.join("state.db");
    let pool = db::connect(&db_path)
        .await
        .context("failed to open Warden's SQLite database")?;

    // Crash recovery runs on every startup, before any new run is
    // considered, per Architecture.md §9 (Disaster Recovery).
    let recovered = orchestrator::recover_crashed_runs(&pool)
        .await
        .context("failed to run crash recovery")?;
    for run_id in &recovered {
        tracing::warn!(
            run_id,
            "run marked Failed on startup: no live process found (crash recovery)"
        );
    }

    let cancel = CancellationToken::new();
    let cancel_on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("received Ctrl-C, cancelling run");
            cancel_on_ctrl_c.cancel();
        }
    });

    let config = RunConfig {
        repo_path: repo,
        warden_home,
        branch,
        intent,
        max_cycles,
        coder_command: parse_agent_command(&coder_cmd)?,
        reviewer_command: parse_agent_command(&reviewer_cmd)?,
        tester_command: parse_agent_command(&tester_cmd)?,
        evidence_tool,
        evidence_store_in_repo,
        gate,
    };

    let orchestrator = Orchestrator::new(pool);
    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, cancel)
        .await
        .context("convergence loop failed")?;

    tracing::info!(run_id, ?final_state, "run finished");
    println!("run {run_id} finished: {final_state:?}");

    Ok(())
}

/// Splits a shell-style command string (e.g. `"claude -p coder.md"`) into
/// program + args. Simple whitespace splitting is enough for Phase 1;
/// agents that need quoting/escaping should be wrapped in their own
/// script.
fn parse_agent_command(raw: &str) -> anyhow::Result<AgentCommand> {
    let mut parts = raw.split_whitespace();
    let program = parts.next().context("agent command must not be empty")?;
    Ok(AgentCommand::new(program, parts))
}

/// clap `value_parser` for `--evidence-tool`: delegates to
/// `warden_core::EvidenceTool::parse` so the CLI and any future config-file
/// parsing validate against the exact same closed set (code-standards.md:
/// "valider toute entrée externe... à la frontière").
fn parse_evidence_tool(raw: &str) -> Result<warden_core::EvidenceTool, String> {
    warden_core::EvidenceTool::parse(raw).map_err(|error| error.to_string())
}

fn default_warden_home() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set; pass --warden-home explicitly")?;
    if home.trim().is_empty() {
        bail!("HOME is empty; pass --warden-home explicitly");
    }
    Ok(PathBuf::from(home).join(".warden"))
}

fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(format!("warden={level},warden_core={level}"))
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
