//! `warden` binary: CLI parsing + dispatch only. All orchestration logic
//! lives in the `warden` library crate (`src/lib.rs` and friends).

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use warden::agent_def::resolve_agent_definition;
use warden::db;
use warden::gate_trigger;
use warden::orchestrator::{self, Orchestrator, RunConfig};
use warden::tool_adapter::{ClaudeAdapter, ToolAdapter};
use warden_core::AgentRole;

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

        /// The task description passed to the coder agent (ADR-0012, issue
        /// #20 Scope B: propagated over the coder's stdin). Must not be
        /// blank -- validated here rather than left to fail deep inside the
        /// first cycle (M2, issue #20 review), where
        /// `AgentInputMessage::for_coder` enforces the same rule.
        #[arg(long, value_parser = parse_intent)]
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

        /// Selects the built-in tool adapter every role runs through this
        /// run (issue #24): the invocation, env allowlist, output-to-
        /// findings translation, and default prompt for all three roles all
        /// come from this one adapter. Global for the whole run --
        /// per-role tool selection (`--coder-tool`...) is out of scope.
        /// Replaces the removed `--coder-agent`/`--reviewer-agent`/
        /// `--tester-agent` flags and the warden-native runner they
        /// selected (ADR-0013, issue #22).
        #[arg(long, value_parser = parse_tool)]
        tool: ToolName,

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

/// The closed set of `--tool` values this build understands (issue #24):
/// each variant owns exactly one [`ToolAdapter`] impl, resolved at compile
/// time -- not a config-declared registry, mirroring
/// `warden_core::AgentRole`/`RunState` string parsing. `claude` is the only
/// variant today; `aider` and others are meant to gain their own variant +
/// adapter later, never by adding a runtime lookup table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolName {
    Claude,
}

/// clap `value_parser` for `--tool`: validated against the closed set above
/// at the CLI boundary (code-standards.md: "valider toute entrée externe...
/// à la frontière"), mirroring `parse_evidence_tool`.
fn parse_tool(raw: &str) -> Result<ToolName, String> {
    match raw {
        "claude" => Ok(ToolName::Claude),
        other => Err(format!("unknown --tool {other:?} (supported: \"claude\")")),
    }
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
            tool,
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

            // One arm today (`ToolName::Claude`); a future adapter gets its
            // own arm here rather than a runtime lookup, so the concrete
            // `R: ToolAdapter` `run_convergence_loop` needs stays resolved
            // at compile time (see `ToolName`'s own docs).
            match tool {
                ToolName::Claude => {
                    run(
                        repo,
                        intent,
                        branch,
                        max_cycles,
                        warden_home,
                        ClaudeAdapter,
                        evidence_tool,
                        evidence_store_in_repo,
                        gate,
                    )
                    .await
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run<R: ToolAdapter>(
    repo: PathBuf,
    intent: String,
    branch: String,
    max_cycles: u32,
    warden_home: Option<PathBuf>,
    adapter: R,
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

    // The Ctrl-C handler is armed before anything else that could itself
    // block startup (issue #15 review, H1(c)) -- otherwise a
    // deterministically-failing/hanging step ahead of it (e.g. the
    // AwaitingCi resume below) would make warden unresponsive to Ctrl-C
    // during that entire window, on top of never reaching the new run at
    // all.
    let cancel = CancellationToken::new();
    let cancel_on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("received Ctrl-C, cancelling run");
            cancel_on_ctrl_c.cancel();
        }
    });

    // Issue #15/ADR-0011 crash-recovery counterpart: any run left stuck in
    // `AwaitingCi` needs its watch re-requested, not treated as a crashed
    // agent process (see `recover_crashed_runs`'s own doc comment) --
    // requires a `GateTrigger`, so only runs when the gate is configured.
    //
    // Issue #15 review, H1(c)/M4: spawned in the background rather than
    // awaited here -- a stuck run's watch can legitimately take up to its
    // own receive timeout to resolve, and none of that may gate this
    // process's own new run from starting.
    if let Some(gate_config) = &gate {
        let trigger = gate_trigger::SubprocessGateTrigger {
            gated_bin: gate_config.gated_bin.clone(),
            db_path: db_path.clone(),
            bare_repo_path: gate_config.bare_repo_path.clone(),
            repo_slug: gate_config.repo_slug.clone(),
            poll_interval_secs: gate_config.poll_interval_secs,
            inactivity_timeout_secs: gate_config.inactivity_timeout_secs,
        };
        let resume_pool = pool.clone();
        let resume_warden_home = warden_home.clone();
        let resume_bare_repo = gate_config.bare_repo_path.clone();
        tokio::spawn(async move {
            match orchestrator::resume_awaiting_ci_runs(
                resume_pool,
                resume_warden_home,
                trigger,
                resume_bare_repo,
            )
            .await
            {
                Ok(resumed) => {
                    for run_id in &resumed {
                        tracing::warn!(
                            run_id,
                            "resumed a run stuck in AwaitingCi (crash recovery)"
                        );
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "failed to resume runs stuck in AwaitingCi");
                }
            }
        });
    }

    // Issue #24: resolved once, from the base repo (`repo`, not any
    // worktree), before this run's `runs` row is even written -- see
    // `warden::agent_def::resolve_agent_definition`'s own docs for why that
    // timing is the security-relevant part of this call.
    let coder_agent = resolve_agent_definition(&repo, AgentRole::Coder, &adapter).await?;
    let reviewer_agent = resolve_agent_definition(&repo, AgentRole::Reviewer, &adapter).await?;
    let tester_agent = resolve_agent_definition(&repo, AgentRole::Tester, &adapter).await?;

    let config = RunConfig {
        repo_path: repo,
        warden_home,
        branch,
        intent,
        max_cycles,
        coder_agent,
        reviewer_agent,
        tester_agent,
        evidence_tool,
        evidence_store_in_repo,
        gate,
    };

    let orchestrator = Orchestrator::new(pool);
    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, adapter, cancel)
        .await
        .context("convergence loop failed")?;

    tracing::info!(run_id, ?final_state, "run finished");
    println!("run {run_id} finished: {final_state:?}");

    Ok(())
}

/// clap `value_parser` for `--evidence-tool`: delegates to
/// `warden_core::EvidenceTool::parse` so the CLI and any future config-file
/// parsing validate against the exact same closed set (code-standards.md:
/// "valider toute entrée externe... à la frontière").
fn parse_evidence_tool(raw: &str) -> Result<warden_core::EvidenceTool, String> {
    warden_core::EvidenceTool::parse(raw).map_err(|error| error.to_string())
}

/// M2 (issue #20 review): rejects a blank/all-whitespace `--intent` at the
/// CLI boundary, with the same rule `AgentInputMessage::for_coder` enforces
/// -- a run started with `--intent ""` would otherwise create its `runs`
/// row, transition to `CoderRunning`, and only then fail once the first
/// cycle tries to build the coder's stdin payload.
fn parse_intent(raw: &str) -> Result<String, String> {
    if raw.trim().is_empty() {
        return Err("run intent must not be blank".to_string());
    }
    Ok(raw.to_string())
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
