//! `warden` binary: CLI parsing + dispatch only. All orchestration logic
//! lives in the `warden` library crate (`src/lib.rs` and friends).

use std::io::Write as _;
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

    // Issue #31: resolved before `warden_home` moves into `config` below --
    // this is the resolved `warden_home` (not the raw `--warden-home` flag,
    // which may be unset), so the printed attach command is copy-pasteable
    // as-is.
    //
    // Review M3: also made absolute, distinct from `warden_home` itself
    // (every other consumer below keeps resolving a relative
    // `--warden-home` against this process's cwd exactly as before -- only
    // the *printed* copy changes). A relative path would otherwise echo
    // verbatim and break as soon as the command is pasted from a different
    // cwd, defeating the whole point of printing it. `std::path::absolute`
    // is purely lexical (prepends the cwd, normalizes `.`/`..`) rather than
    // `canonicalize`, which would also resolve symlinks and require the
    // path to already exist -- `warden_home` (e.g. the default
    // `~/.warden`) routinely doesn't exist yet at this point. A failure
    // here (cwd unreadable) reflects an already-degraded environment this
    // print can't fix either way, so it falls back to the unresolved path
    // rather than failing the whole run over a cosmetic concern.
    let attach_warden_home =
        std::path::absolute(&warden_home).unwrap_or_else(|_| warden_home.clone());

    // Review M1: `--warden-home`'s value is interpolated into the printed
    // `warden-tui attach` command, so it must be shell-quoted the same way
    // `evidence.rs::shell_join` quotes `asciinema`'s record command (same
    // `shlex::try_quote` convention) -- otherwise a warden_home containing
    // a space or other shell metacharacter produces a line that breaks on
    // paste, which is precisely the "copiable telle quelle" requirement
    // this feature exists for. Resolved once, eagerly, here (not inside the
    // `on_run_started` callback below) so a genuine failure -- the resolved
    // path is not valid UTF-8, and thus cannot be made copy-pasteable at
    // all -- fails this command clearly before any run starts, rather than
    // being silently swallowed mid-run where nothing could surface it.
    let attach_warden_home_quoted =
        shlex::try_quote(attach_warden_home.to_str().with_context(|| {
            format!(
                "--warden-home ({}) is not valid UTF-8; cannot render a copy-pasteable \
                 `warden-tui attach` command",
                attach_warden_home.display()
            )
        })?)
        .map(|quoted| quoted.into_owned())
        // `shlex::try_quote` only ever fails on an embedded NUL byte, which
        // `to_str()` above already would have rejected as invalid UTF-8 first --
        // this arm is unreachable in practice, kept only so a future change to
        // either check still fails loudly instead of silently.
        .context("--warden-home cannot be shell-quoted (embedded NUL byte)")?;

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

    // Printed at run start (not via `tracing`, so it shows at the default
    // `warn` verbosity) rather than only once the run finishes, so
    // `warden-tui attach` can follow a live run without the user having to
    // query SQLite by hand for its run_id.
    //
    // Review L2: `--run-id`'s value below is `run_id` itself, always
    // `Uuid::new_v4().to_string()` (see `Orchestrator::on_run_started`'s
    // docs) -- lowercase hex and hyphens only, never containing shell
    // metacharacters -- so, unlike `attach_warden_home_quoted` above, it
    // does not need its own `shlex::try_quote` pass.
    let orchestrator = Orchestrator::new(pool).on_run_started(move |run_id| {
        print_run_started_hint(run_id, &attach_warden_home_quoted);
    });
    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, adapter, cancel)
        .await
        .context("convergence loop failed")?;

    tracing::info!(run_id, ?final_state, "run finished");
    // Review L2: same closed-stdout hazard as `print_run_started_hint`,
    // reproduced against the real binary with plain `warden run | head -1`
    // -- see `print_stdout_line_or_log`'s own docs.
    print_stdout_line_or_log(&format!("run {run_id} finished: {final_state:?}"));

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

/// Prints the two `warden run`-start lines (issue #31) through a locked
/// stdout handle instead of `println!`.
///
/// Review L2: `on_run_started` (see its doc comment on [`Orchestrator`])
/// runs synchronously, *mid-run* -- a panic here would unwind through the
/// convergence loop and abort the whole process with the `runs` row stuck
/// in a non-terminal state, a strictly worse outcome than the end-of-run
/// print this callback runs alongside ever risked. See
/// `print_stdout_line_or_log`'s own docs for why a closed pipe (e.g.
/// `warden run | head -1`) can't panic either print.
fn print_run_started_hint(run_id: &str, quoted_warden_home: &str) {
    print_stdout_line_or_log(&format!("run {run_id} started"));
    print_stdout_line_or_log(&format!(
        "attach: warden-tui attach --run-id {run_id} --warden-home {quoted_warden_home}"
    ));
}

/// Writes `line` + a newline to stdout through a locked handle, in place of
/// `println!`, which panics outright if stdout is closed (e.g. `warden run
/// | head -1` -- reproduced against the real binary in the issue #31
/// review, both for the mid-run `on_run_started` hint and for the
/// pre-existing end-of-run line below).
///
/// A `BrokenPipe` write error is the one swallowed deliberately here: every
/// caller of this function prints an advisory status line the run's own
/// correctness never depends on reaching a terminal, so losing one to a
/// reader that already hung up is not a reason to crash an otherwise
/// successful (or, worse, still-live) run. Any other write error is logged
/// instead of silently dropped, since that would signal something less
/// routine than a closed pipe.
fn print_stdout_line_or_log(line: &str) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if let Err(error) = writeln!(handle, "{line}") {
        if error.kind() != std::io::ErrorKind::BrokenPipe {
            tracing::warn!(%error, "failed to print to stdout");
        }
    }
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
