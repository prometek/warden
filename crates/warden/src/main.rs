//! `warden` binary: CLI parsing + dispatch only. All orchestration logic
//! lives in the `warden` library crate (`src/lib.rs` and friends).

use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use warden::agent_def::resolve_agent_definition;
use warden::db;
use warden::gate_trigger;
use warden::hook_config::load_repo_hooks;
use warden::orchestrator::{self, Orchestrator, RunConfig};
use warden::tool_adapter::{ClaudeAdapter, CodexAdapter, MistralAdapter, ToolAdapter};
use warden_core::AgentRole;
use warden_sandbox::{LocalSandbox, Sandbox};

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

        /// Maximum number of coder<->reviewer round trips before giving up
        /// (`RunState::StepCyclesExceeded(1)`, issue #43/ADR-0014). Must be
        /// at least 1 — a budget of 0 could never let the coder run at all.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
        max_review_cycles: u32,

        /// Maximum number of times the tester may run and come back with a
        /// blocking finding before giving up (`RunState::StepCyclesExceeded(2)`,
        /// issue #43/ADR-0014). Must be at least 1.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
        max_test_cycles: u32,

        /// Issue #73: the single shared cycle budget for any workflow step
        /// beyond the built-in reviewer/tester pair (`.warden/workflow.yaml`,
        /// e.g. a custom `techlead` step) before giving up
        /// (`RunState::StepCyclesExceeded`). Ignored when the run's workflow
        /// has no such extra step (the built-in default workflow never
        /// does). Must be at least 1.
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

        /// Issue #26: opts into honouring a repo-supplied reviewer/tester
        /// definition (`<repo>/.warden/agents/{reviewer,tester}.md`) when no
        /// user-config definition exists for that role. Off by default -- a
        /// repo's own reviewer/tester convention file is otherwise ignored
        /// entirely, since it is committable by the very coder that role
        /// exists to judge independently (see `warden::agent_def`'s own
        /// "Security: role-asymmetric resolution" docs). When this actually
        /// causes a repo file to be used, it is surfaced as untrusted: a
        /// `tracing::warn!` naming the path, and a
        /// `RunEvent::UntrustedAgentDefinitionUsed` on the run's own event
        /// log. Never affects the coder's own convention file, which was
        /// already read from the repo regardless of this flag.
        #[arg(long)]
        trust_repo_agents: bool,

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

        /// Issue #32: spawns `warden-tui attach` as a separate process
        /// (ADR-0008), in the foreground on this same launch terminal, once
        /// the run starts -- the "launch and watch" flow without manually
        /// copying the `warden-tui attach` hint into a second terminal.
        /// Exiting the TUI for any reason (`q`/`Esc`/Ctrl-C, or a crash)
        /// cancels this run: there is no other channel back from the
        /// read-only TUI to tell `warden run` "just detach, keep going".
        #[arg(long)]
        tui: bool,

        /// Overrides the `warden-tui` binary `--tui` spawns. Defaults to
        /// looking for `warden-tui` next to this running `warden` binary
        /// (the usual co-installed-workspace-binaries layout), then falling
        /// back to a `PATH` lookup. Ignored unless `--tui` is also set.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        tui_bin: Option<PathBuf>,

        /// Selects the [`warden_sandbox::Sandbox`] backend every agent
        /// invocation in this run goes through (issue #49, ADR-0015/
        /// ADR-0019): `worktree` (default) is `warden_sandbox::LocalSandbox`
        /// -- unchanged from every `warden run` before this flag existed,
        /// the agent's own process runs directly on this host. `docker` is
        /// `warden_sandbox::DockerSandbox` -- each invocation runs inside a
        /// container instead, with the role's own worktree and the base
        /// repo's `.git` bind-mounted read-write, `~/.claude` bind-mounted
        /// read-only for auth, and nothing else of the host reachable (see
        /// `warden_sandbox::docker`'s own docs for the exact guarantees and
        /// the accepted v1 limits: no egress filtering yet).
        #[arg(long, default_value = "worktree", value_parser = parse_isolation)]
        isolation: Isolation,

        /// Overrides the image `--isolation docker` runs every agent
        /// invocation in. Ignored unless `--isolation docker` is also set;
        /// see `crates/warden-sandbox/docker/README.md` for how to build the
        /// reference image this defaults to.
        #[arg(long, default_value = DEFAULT_DOCKER_IMAGE)]
        isolation_image: String,
    },
}

/// The closed set of `--isolation` values this build understands (issue
/// #49): mirrors [`ToolName`]/[`parse_tool`]'s own closed-set pattern.
/// `Worktree` selects `warden_sandbox::LocalSandbox` (the default, unchanged
/// behaviour); `Docker` selects `warden_sandbox::DockerSandbox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Isolation {
    Worktree,
    Docker,
}

/// clap `value_parser` for `--isolation`: validated against the closed set
/// above at the CLI boundary (code-standards.md: "valider toute entrée
/// externe... à la frontière"), mirroring `parse_tool`.
fn parse_isolation(raw: &str) -> Result<Isolation, String> {
    match raw {
        "worktree" => Ok(Isolation::Worktree),
        "docker" => Ok(Isolation::Docker),
        other => Err(format!(
            "unknown --isolation {other:?} (supported: \"worktree\", \"docker\")"
        )),
    }
}

/// Default image `--isolation docker` runs every agent invocation in --
/// built from `crates/warden-sandbox/docker/Dockerfile` (issue #49). No
/// separate `--isolation-image` is required for the common case; the flag
/// exists only to override it.
const DEFAULT_DOCKER_IMAGE: &str = "warden-agent:latest";

/// The closed set of `--tool` values this build understands (issue #24,
/// extended to `codex`/`mistral` by issue #71): each variant owns exactly
/// one [`ToolAdapter`] impl, resolved at compile time -- not a
/// config-declared registry, mirroring `warden_core::AgentRole`/`RunState`
/// string parsing. Other CLIs are meant to gain their own variant + adapter
/// later, never by adding a runtime lookup table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolName {
    Claude,
    Codex,
    Mistral,
}

/// clap `value_parser` for `--tool`: validated against the closed set above
/// at the CLI boundary (code-standards.md: "valider toute entrée externe...
/// à la frontière"), mirroring `parse_evidence_tool`.
fn parse_tool(raw: &str) -> Result<ToolName, String> {
    match raw {
        "claude" => Ok(ToolName::Claude),
        "codex" => Ok(ToolName::Codex),
        "mistral" => Ok(ToolName::Mistral),
        other => Err(format!(
            "unknown --tool {other:?} (supported: \"claude\", \"codex\", \"mistral\")"
        )),
    }
}

/// Issue #49: `--isolation`/`--isolation-image` bundled into one config,
/// resolved once here (not inside `run`), the same shape `GateConfig`/
/// `TuiLaunchConfig` above already use for their own flag pairs.
struct IsolationConfig {
    isolation: Isolation,
    image: String,
}

/// A newtype around `--trust-repo-agents`'s `bool` (issue #26 review, LOW):
/// `run`'s own parameter list carries this alongside `evidence_store_in_repo`
/// (also a bare `bool`), separated only by a generic `adapter` and an
/// `Option<EvidenceTool>` -- a future insertion there could silently
/// transpose the two positionally, and this one is a security-relevant
/// switch (it gates whether a reviewer/tester definition the coder can write
/// to is ever used at all). Wrapping it in its own type makes that
/// transposition a compile error instead of a silent bug.
#[derive(Debug, Clone, Copy)]
struct TrustRepoAgents(bool);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Commands::Run {
            repo,
            intent,
            branch,
            max_review_cycles,
            max_test_cycles,
            max_cycles,
            warden_home,
            tool,
            trust_repo_agents,
            evidence_tool,
            evidence_store_in_repo,
            gate_bare_repo,
            gate_gated_bin,
            gate_repo_slug,
            gate_poll_interval_secs,
            gate_inactivity_timeout_secs,
            tui,
            tui_bin,
            isolation,
            isolation_image,
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

            // Issue #32: `--tui-bin` is only meaningful alongside `--tui`;
            // resolved once here (not inside `run`), same shape as `gate`
            // above.
            let tui_launch = tui.then(|| TuiLaunchConfig {
                tui_bin: resolve_tui_binary(tui_bin),
            });

            // Issue #49: bundled the same way as `gate`/`tui_launch` above.
            let isolation_config = IsolationConfig {
                isolation,
                image: isolation_image,
            };

            // Three arms today (`ToolName::{Claude,Codex,Mistral}`, issue
            // #71); a future adapter gets its own arm here rather than a
            // runtime lookup, so the concrete `R: ToolAdapter`
            // `run_convergence_loop` needs stays resolved at compile time
            // (see `ToolName`'s own docs).
            match tool {
                ToolName::Claude => {
                    run(
                        repo,
                        intent,
                        branch,
                        max_review_cycles,
                        max_test_cycles,
                        max_cycles,
                        warden_home,
                        ClaudeAdapter,
                        TrustRepoAgents(trust_repo_agents),
                        evidence_tool,
                        evidence_store_in_repo,
                        gate,
                        tui_launch,
                        isolation_config,
                    )
                    .await
                }
                ToolName::Codex => {
                    run(
                        repo,
                        intent,
                        branch,
                        max_review_cycles,
                        max_test_cycles,
                        max_cycles,
                        warden_home,
                        CodexAdapter,
                        TrustRepoAgents(trust_repo_agents),
                        evidence_tool,
                        evidence_store_in_repo,
                        gate,
                        tui_launch,
                        isolation_config,
                    )
                    .await
                }
                ToolName::Mistral => {
                    run(
                        repo,
                        intent,
                        branch,
                        max_review_cycles,
                        max_test_cycles,
                        max_cycles,
                        warden_home,
                        MistralAdapter,
                        TrustRepoAgents(trust_repo_agents),
                        evidence_tool,
                        evidence_store_in_repo,
                        gate,
                        tui_launch,
                        isolation_config,
                    )
                    .await
                }
            }
        }
    }
}

/// Issue #32: resolved once in `main`, before `run` is called -- same shape
/// as `orchestrator::GateConfig`.
struct TuiLaunchConfig {
    tui_bin: PathBuf,
}

/// Resolves the `warden-tui` binary `--tui` spawns (issue #32).
///
/// `--tui-bin`, if given, always wins. Otherwise, looks for `warden-tui` next
/// to this running `warden` binary -- the layout `cargo build --release`
/// (or any install that keeps the workspace's `[[bin]]`s together) produces
/// -- falling back to a bare `warden-tui` name, which `spawn_tui_attach`'s
/// `Command::new` resolves against `PATH` the normal way. That last fallback
/// is not validated here: a binary genuinely missing from both places
/// surfaces as `spawn_tui_attach`'s own typed `ProcessError::Spawn` once
/// actually invoked, naming this exact path, rather than being pre-empted by
/// a duplicate check here that could itself race a `PATH` that changes
/// between resolution and spawn.
fn resolve_tui_binary(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(explicit) = explicit {
        return explicit;
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let sibling = dir.join(format!("warden-tui{}", std::env::consts::EXE_SUFFIX));
            if sibling.is_file() {
                return sibling;
            }
        }
    }

    PathBuf::from(format!("warden-tui{}", std::env::consts::EXE_SUFFIX))
}

/// Relative path `.warden/workflow.yaml` resolves against a run's repo
/// (issue #73) -- mirrors `agent_def::AGENTS_DIR`'s own convention of a
/// dotfile under the repo root.
const WORKFLOW_FILE: &str = ".warden/workflow.yaml";

/// Loads and validates this run's pipeline (issue #73): `.warden/workflow.yaml`
/// if present, else `Workflow::builtin_default()` -- the latter is what
/// makes a run with no workflow file reproduce the pre-issue-#73 pipeline
/// exactly (strict retro-compat).
///
/// **Current engine limitation, enforced here (not a silent restriction):**
/// the convergence loop's built-in coder/reviewer/tester steps still run
/// through their own existing, hardened path (`agent_def::resolve_agent_definition`'s
/// role-asymmetric trust model) -- a custom `workflow.yaml` may only
/// *append* steps after them, never reorder, replace, or omit them. A
/// workflow whose first three steps aren't exactly `coder`, `reviewer`,
/// `tester` (in that order) is rejected with a clear error naming the
/// mismatch, rather than silently running something the loop cannot
/// actually execute.
///
/// Every step beyond those three is resolved via
/// `agent_def::resolve_custom_step_agent_definition` (`.claude/agents/<agent>.md`,
/// ADR-0013) -- an unresolvable agent for any such step fails the run here,
/// before anything is spawned, naming the role and the exact path expected.
async fn load_workflow(
    repo: &std::path::Path,
) -> anyhow::Result<(warden_core::Workflow, Vec<warden_core::AgentDefinition>)> {
    let workflow_path = repo.join(WORKFLOW_FILE);
    let workflow = match tokio::fs::read_to_string(&workflow_path).await {
        Ok(raw) => warden_core::Workflow::parse_yaml(&raw)
            .with_context(|| format!("invalid workflow file at {}", workflow_path.display()))?,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            warden_core::Workflow::builtin_default()
        }
        Err(source) => {
            return Err(source).with_context(|| {
                format!(
                    "failed to read workflow file at {}",
                    workflow_path.display()
                )
            })
        }
    };

    let builtin_role_names: Vec<&str> = workflow
        .steps
        .iter()
        .take(3)
        .map(|step| step.role.as_str())
        .collect();
    if builtin_role_names != ["coder", "reviewer", "tester"] {
        bail!(
            "{}: the first three steps must be exactly \"coder\", \"reviewer\", \"tester\" (in \
             that order) -- found {:?}. The current engine only supports appending custom steps \
             after this built-in pipeline, not reordering, replacing, or omitting it.",
            workflow_path.display(),
            builtin_role_names
        );
    }

    let mut extra_step_agents = Vec::with_capacity(workflow.steps.len().saturating_sub(3));
    for step in workflow.steps.iter().skip(3) {
        let definition = warden::agent_def::resolve_custom_step_agent_definition(
            repo,
            step.role.as_str(),
            &step.agent,
        )
        .await
        .with_context(|| {
            format!(
                "failed to resolve the agent for custom workflow role {:?} (agent {:?})",
                step.role.as_str(),
                step.agent
            )
        })?;
        extra_step_agents.push(definition);
    }

    Ok((workflow, extra_step_agents))
}

#[allow(clippy::too_many_arguments)]
async fn run<R: ToolAdapter>(
    repo: PathBuf,
    intent: String,
    branch: String,
    max_review_cycles: u32,
    max_test_cycles: u32,
    max_cycles: u32,
    warden_home: Option<PathBuf>,
    adapter: R,
    trust_repo_agents: TrustRepoAgents,
    evidence_tool: Option<warden_core::EvidenceTool>,
    evidence_store_in_repo: bool,
    gate: Option<orchestrator::GateConfig>,
    tui_launch: Option<TuiLaunchConfig>,
    isolation_config: IsolationConfig,
) -> anyhow::Result<()> {
    // Issue #26 review: `Option::unwrap_or` (the previous form here)
    // evaluates its argument eagerly, so `default_warden_home()?` used to
    // run -- and could fail on a missing `HOME` -- even when `--warden-home`
    // was passed explicitly and its result would just be discarded. This
    // `match` only calls `default_warden_home()` when `warden_home` is
    // actually `None`, matching the flag's own documented "defaults to
    // `~/.warden`" behaviour instead of silently requiring `HOME`
    // unconditionally.
    let warden_home = match warden_home {
        Some(warden_home) => warden_home,
        None => default_warden_home()?,
    };
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
    //
    // Issue #26: the reviewer/tester's own trusted source
    // (`user_config_agents_dir`) is resolved once here, from this process's
    // real environment (`XDG_CONFIG_HOME`/`HOME`) -- see
    // `agent_def::default_user_config_agents_dir`'s own docs for why that
    // env read lives here rather than inside `resolve_agent_definition`
    // itself. `warden_home` is passed alongside it (owner's ruling,
    // "escalated asymmetry"): a user-config source resolving under
    // `<warden_home>/worktrees/` -- a stale worktree from a crashed run --
    // must be degraded exactly like one resolving inside the repo itself.
    let user_config_agents_dir = warden::agent_def::default_user_config_agents_dir()?;
    let (coder_agent, _coder_source) = resolve_agent_definition(
        &repo,
        AgentRole::Coder,
        &adapter,
        &user_config_agents_dir,
        &warden_home,
        trust_repo_agents.0,
    )
    .await?;
    let (reviewer_agent, reviewer_source) = resolve_agent_definition(
        &repo,
        AgentRole::Reviewer,
        &adapter,
        &user_config_agents_dir,
        &warden_home,
        trust_repo_agents.0,
    )
    .await?;
    let (tester_agent, tester_source) = resolve_agent_definition(
        &repo,
        AgentRole::Tester,
        &adapter,
        &user_config_agents_dir,
        &warden_home,
        trust_repo_agents.0,
    )
    .await?;

    // Issue #73: `.warden/workflow.yaml`, if present, defines this run's
    // pipeline; its absence reproduces the pre-issue-#73 pipeline exactly
    // (`Workflow::builtin_default`) -- the strict retro-compat requirement
    // this whole feature is judged against.
    let (workflow, extra_step_agents) = load_workflow(&repo).await?;

    // Issue #26: `resolve_agent_definition` already `tracing::warn!`ed the
    // moment it actually read a repo-sourced reviewer/tester definition
    // (before this run, or its Event Bus, even exist) -- this just collects
    // which role(s) that happened for, so `run_convergence_loop` can also
    // publish a persisted `RunEvent::UntrustedAgentDefinitionUsed` for each,
    // once the run's own event log exists to carry it.
    let untrusted_repo_agent_definitions = [
        (AgentRole::Reviewer, reviewer_source),
        (AgentRole::Tester, tester_source),
    ]
    .into_iter()
    .filter_map(|(role, source)| match source {
        warden::agent_def::AgentDefinitionSource::UntrustedRepoOverride {
            path,
            canonical_path,
        } => Some(orchestrator::UntrustedRepoAgentDefinition {
            role,
            path,
            canonical_path,
        }),
        _ => None,
    })
    .collect();

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
        max_review_cycles,
        max_test_cycles,
        workflow,
        max_extra_step_cycles: max_cycles,
        coder_agent,
        reviewer_agent,
        tester_agent,
        evidence_tool,
        evidence_store_in_repo,
        gate,
        extra_step_agents,
        untrusted_repo_agent_definitions,
    };

    // Issue #49: `--isolation` selects the `Sandbox` backend every agent
    // invocation in this run goes through. `Isolation::Worktree` is
    // `Orchestrator::new`'s own default (`LocalSandbox`) and needs no
    // override; `Isolation::Docker` builds a `DockerSandbox` bound to this
    // run's own base repo (`config.repo_path`, not any role's own worktree --
    // that arrives per-invocation via `SandboxSpec::cwd`, exactly like
    // `LocalSandbox`) and the host's `~/.claude` (resolved here, not inside
    // `warden_sandbox`, so a missing `HOME` is this same "pass
    // `--warden-home` explicitly"-style error `default_warden_home` already
    // uses, not a sandbox-layer one).
    let sandbox: Option<std::sync::Arc<dyn warden_sandbox::Sandbox>> =
        match isolation_config.isolation {
            Isolation::Worktree => None,
            Isolation::Docker => {
                let claude_config_dir = default_claude_config_dir()?;
                Some(std::sync::Arc::new(warden_sandbox::DockerSandbox::new(
                    warden_sandbox::DockerConfig {
                        image: isolation_config.image,
                        repo_path: config.repo_path.clone(),
                        claude_config_dir,
                    },
                )))
            }
        };

    let cancel_on_tui_exit = cancel.clone();

    // Issue #32 review (HIGH): holds the `JoinHandle` for the task that
    // awaits the spawned `warden-tui` child (see below), set from inside
    // `on_run_started` -- that callback must stay synchronous/non-blocking
    // (its own docs), so it cannot itself await the child. `run` awaits this
    // same handle once the convergence loop below has settled (see the
    // comment at that await site for why).
    let tui_watcher: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let tui_watcher_setter = tui_watcher.clone();

    // Issue #32 review (MEDIUM): a `--tui` spawn failure recorded here, then
    // checked -- and, if present, surfaced as this run's own failure --
    // right after the convergence loop below settles. `--tui` is an
    // explicit user request for a spawned, attached `warden-tui` (including
    // the cancel-on-exit safety net it provides); silently continuing the
    // run headless when that spawn fails would both drop the feature the
    // user asked for and violate code-standards.md's "no silent fallback".
    let tui_spawn_error: std::sync::Arc<std::sync::Mutex<Option<warden::error::ProcessError>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let tui_spawn_error_setter = tui_spawn_error.clone();

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
    // Issue #49's agent-isolation sandbox: `None` => the orchestrator's own
    // default `LocalSandbox` (unchanged `--isolation worktree` behaviour);
    // `Some` => the `DockerSandbox` that `--isolation docker` selected above.
    let mut orchestrator = Orchestrator::new(pool);
    if let Some(sandbox) = sandbox {
        orchestrator = orchestrator.with_sandbox(sandbox);
    }

    // Lifecycle hooks run on the HOST, never inside an agent's isolation
    // container: they are the operator's own infra prep (`docker compose up`,
    // `git pull`) against the repo as a whole, so they always go through a
    // LocalSandbox regardless of `--isolation`. Absent `.warden/hooks.toml` =>
    // empty registry (dispatch stays a no-op). See `warden::hook_config` for
    // the trust model: a repo's hook commands are honoured by default,
    // consistent with its `.warden/agents/coder.md`.
    let hook_sandbox: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new());
    let hooks = load_repo_hooks(&config.repo_path, hook_sandbox)
        .context("failed to load .warden/hooks.toml")?;

    let orchestrator = orchestrator
        .with_hooks(hooks)
        .on_run_started(move |run_id| {
            print_run_started_hint(run_id, &attach_warden_home_quoted);

            // Issue #32: `--tui` spawns `warden-tui attach` as a separate
            // process (ADR-0008), in the foreground on this launch terminal,
            // once the run_id it needs actually exists. `Command::spawn` (used
            // by `spawn_tui_attach`) is itself synchronous/non-blocking -- it
            // only issues the `fork`/`exec` syscalls and returns -- so calling
            // it directly here does not violate `on_run_started`'s "must not
            // block" contract.
            if let Some(tui_launch) = &tui_launch {
                match warden::process::spawn_tui_attach(
                    &tui_launch.tui_bin,
                    run_id,
                    &attach_warden_home,
                ) {
                    Ok(child) => {
                        let cancel_on_tui_exit = cancel_on_tui_exit.clone();
                        let handle = tokio::spawn(async move {
                            cancel_run_when_tui_exits(child, cancel_on_tui_exit).await;
                        });
                        *tui_watcher_setter
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
                    }
                    Err(error) => {
                        tracing::error!(
                            %error,
                            tui_bin = %tui_launch.tui_bin.display(),
                            "failed to spawn warden-tui for --tui; aborting the run"
                        );
                        // Issue #32 review (MEDIUM): abort immediately, right
                        // here, rather than only once the convergence loop below
                        // eventually returns -- the coder's very first
                        // invocation hasn't even started yet at this point
                        // (`on_run_started` fires before any per-cycle work,
                        // see its own docs), so cancelling now stops the run
                        // from doing any real work at all instead of running an
                        // entire (headless) cycle it will just fail after.
                        *tui_spawn_error_setter
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
                        cancel_on_tui_exit.cancel();
                    }
                }
            }
        });
    let convergence_result = orchestrator
        .run_convergence_loop(config, adapter, cancel)
        .await;

    // Issue #32 review (HIGH, then re-review): whether to wait here for a
    // still-attached `warden-tui` to exit before deciding this run's own
    // outcome below depends on whether *this process's own* stdout is a
    // terminal -- see `should_wait_for_spawned_tui`'s own docs for exactly
    // why that (rather than always/never waiting) is the correct gate.
    // `spawn_tui_attach` inherits stdio, so the spawned `warden-tui`'s own
    // `is_terminal(stdout)` check always agrees with this one.
    //
    // If the TUI already exited on its own (having triggered
    // `cancel_run_when_tui_exits`'s `cancel.cancel()`, which is what caused
    // the convergence loop above to end early), awaiting it here resolves
    // immediately -- that task's own last action is exactly the `cancel()`
    // call that unblocks the loop, so by the time this is reached it has
    // already finished. If `--tui` was never set, `tui_watcher` stays
    // `None` and this is a no-op either way.
    if should_wait_for_spawned_tui(std::io::stdout().is_terminal()) {
        let tui_watcher_handle = tui_watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = tui_watcher_handle {
            let _ = handle.await;
        }
    }

    // Issue #32 review (MEDIUM): a recorded `--tui` spawn failure is this
    // run's own root cause and takes precedence over whatever the
    // convergence loop itself returned (typically just
    // `ProcessError::Cancelled`, the downstream symptom of the
    // `cancel.cancel()` the spawn failure triggered above) -- surfaced with
    // its own actionable message (already naming the resolved `--tui-bin`
    // path, see `ProcessError::Spawn`'s `Display`) rather than the more
    // generic "cancelled" one.
    if let Some(spawn_error) = tui_spawn_error
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        return Err(spawn_error).context("failed to spawn warden-tui for --tui; aborted the run");
    }

    let (run_id, final_state) = convergence_result.context("convergence loop failed")?;

    tracing::info!(run_id, ?final_state, "run finished");
    // Review L2: same closed-stdout hazard as `print_run_started_hint`,
    // reproduced against the real binary with plain `warden run | head -1`
    // -- see `print_stdout_line_or_log`'s own docs.
    print_stdout_line_or_log(&format!("run {run_id} finished: {final_state:?}"));

    Ok(())
}

/// Issue #32 decision ("la sortie de la TUI annule le run"): awaits the
/// `warden-tui` child spawned for `--tui`, then cancels the run regardless of
/// *why* it exited -- clean quit (`q`/`Esc`/Ctrl-C, see `warden_tui`'s own
/// `is_quit`), a crash, or being killed directly. There is no channel back
/// from the read-only TUI (ADR-0008) to distinguish "detach, keep the run
/// going" from "cancel it" -- exit is the only signal this process ever
/// gets, so it is treated uniformly. Cancelling after the run has already
/// reached a terminal state is a harmless no-op (`CancellationToken::cancel`
/// is idempotent, and nothing is left to kill) -- exactly the case where the
/// user keeps a TUI open to watch a run that has already converged, then
/// quits it themselves; `run` awaits this task's own `JoinHandle` afterwards
/// (see the review (HIGH) comment at that await site) precisely so it stays
/// alive until that happens.
async fn cancel_run_when_tui_exits(mut child: tokio::process::Child, cancel: CancellationToken) {
    match child.wait().await {
        Ok(status) => {
            tracing::info!(?status, "warden-tui exited; cancelling the run (issue #32)");
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "failed to wait for the warden-tui child; cancelling the run anyway (issue #32)"
            );
        }
    }
    cancel.cancel();
}

/// Issue #32 re-review: whether `run` should wait for a still-attached
/// `warden-tui` (spawned for `--tui`) to exit before returning, given
/// whether *this process's own* stdout is a terminal. `spawn_tui_attach`
/// inherits stdio, so the spawned `warden-tui`'s own `is_terminal(stdout)`
/// check always agrees with `stdout_is_terminal` here -- making it the exact
/// discriminator between the two modes `warden-tui attach` runs in
/// (`crates/warden-tui/src/main.rs`), which need opposite answers:
///
/// - **tty** (interactive `app_loop`): holds the tty in raw mode/the
///   alternate screen and never self-exits on its own -- it only ever
///   returns via `is_quit` (`q`/`Esc`/Ctrl-C) or its input thread ending
///   (see that module's own docs). `run` must wait (`true`), or `warden`
///   would exit while `warden-tui` still owns the terminal, corrupting it.
/// - **non-tty** (headless `run_headless`, the scriptable NDJSON dump
///   documented for e.g. `warden run --tui > events.ndjson`): self-exits
///   only once its live channel closes, which only happens once the
///   `EventBus`'s `broadcast::Sender` -- held by this very process -- is
///   dropped, which only happens once `run` returns. Waiting here (`true`)
///   would make `run` wait on `warden-tui` waiting on `run` to return: a
///   real, previously-hit deadlock (`warden run --tui` with redirected
///   stdout hangs forever). So this must be `false` in that case --
///   `warden-tui` cleans up on its own once this process's exit closes its
///   socket, exactly as it did before this issue existed.
///
/// Note for anyone piping/capturing this process's own stdout (e.g. `warden
/// run --tui | tee log`, or a test harness reading it to EOF): because
/// `spawn_tui_attach` inherits stdio, the headless `warden-tui` also holds
/// its own copy of that same pipe's write end for as long as it stays
/// alive -- a downstream reader waiting for EOF on it won't see one until
/// *that* process closes its copy too, not merely once this one exits. That
/// is bounded (not another indefinite hang): once this process's own exit
/// closes its end of the Event Bus socket, the real `warden-tui` notices its
/// live channel close and self-exits promptly on its own -- unlike a fake
/// stand-in that just sleeps unconditionally, which is why a test double for
/// "the TUI hasn't exited" shouldn't rely on reading this process's stdout
/// to completion to prove `run` itself didn't wait for it.
fn should_wait_for_spawned_tui(stdout_is_terminal: bool) -> bool {
    stdout_is_terminal
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

/// Resolves the host's Claude Code login/config directory (issue #49,
/// `--isolation docker`) -- `~/.claude`, the one host path `DockerSandbox`
/// bind-mounts read-only for auth (see `warden_sandbox::docker`'s own docs).
/// Same "fail clearly, no silent fallback" shape as `default_warden_home`:
/// a missing/empty `HOME` is this run's own configuration error, not
/// something `--isolation docker` can proceed without.
fn default_claude_config_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME")
        .context("HOME is not set; cannot resolve ~/.claude for --isolation docker")?;
    if home.trim().is_empty() {
        bail!("HOME is empty; cannot resolve ~/.claude for --isolation docker");
    }
    Ok(PathBuf::from(home).join(".claude"))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #32 re-review: pins down `should_wait_for_spawned_tui`'s gate
    /// in isolation, without needing a real pty (impractical in this test
    /// harness -- `assert_cmd` never gives a spawned binary a real
    /// terminal) -- see its own docs for why a tty must wait and a non-tty
    /// must not.
    #[test]
    fn should_wait_for_spawned_tui_is_gated_on_stdout_being_a_terminal() {
        assert!(
            should_wait_for_spawned_tui(true),
            "an interactive warden-tui (real tty) never self-exits -- warden must wait for it"
        );
        assert!(
            !should_wait_for_spawned_tui(false),
            "a headless warden-tui (non-tty) only self-exits once warden's own process drops \
             the Event Bus -- waiting here would deadlock"
        );
    }

    /// Issue #32: `--tui-bin`, when given, must always win over any
    /// sibling-binary/`PATH` auto-detection -- this is the branch every
    /// `--tui`/`--tui-bin` CLI test in `cli.rs` actually exercises (they all
    /// pass an explicit `--tui-bin`), but `resolve_tui_binary` itself had no
    /// direct unit coverage of its own branching before this test.
    #[test]
    fn resolve_tui_binary_prefers_the_explicit_override_when_given() {
        let explicit = PathBuf::from("/some/explicit/path/to/warden-tui");
        assert_eq!(resolve_tui_binary(Some(explicit.clone())), explicit);
    }

    /// Issue #32: with no `--tui-bin`, `resolve_tui_binary` must fall back to
    /// a bare `warden-tui` name (left for `spawn_tui_attach`'s own
    /// `Command::new` to resolve against `PATH`) when no `warden-tui` binary
    /// sits next to the *current* executable.
    ///
    /// Deterministic under `cargo test` without needing to fake
    /// `std::env::current_exe()` (not injectable/mockable here without
    /// refactoring production code purely for testability): a test binary's
    /// own `current_exe()` always resolves under `target/.../deps/`, a
    /// different directory than where compiled `[[bin]]` outputs like
    /// `target/.../warden-tui` actually land -- so the sibling-lookup branch
    /// reliably misses in this harness, and the fallback below is exercised
    /// for real, not merely assumed.
    #[test]
    fn resolve_tui_binary_falls_back_to_a_bare_name_when_no_sibling_binary_exists() {
        let current_exe = std::env::current_exe().expect("current_exe available under cargo test");
        let sibling = current_exe
            .parent()
            .expect("current_exe has a parent dir")
            .join(format!("warden-tui{}", std::env::consts::EXE_SUFFIX));
        assert!(
            !sibling.is_file(),
            "test assumption violated: a real warden-tui binary exists at {} (this test's own \
             directory, not the compiled [[bin]] output directory) -- resolve_tui_binary would \
             then legitimately return that sibling instead of the bare-name fallback this test \
             asserts on: {sibling:?}",
            sibling.display()
        );

        assert_eq!(
            resolve_tui_binary(None),
            PathBuf::from(format!("warden-tui{}", std::env::consts::EXE_SUFFIX))
        );
    }

    /// Issue #71: `--tool` accepts `codex`/`mistral` alongside `claude`,
    /// each resolving to its own closed-set variant (see `ToolName`'s own
    /// docs) -- the CLI-level equivalent of `e2e_an_unknown_tool_is_a_clean_
    /// cli_error_naming_the_value` in `tests/cli.rs`, but for the parser
    /// itself rather than the whole binary.
    #[test]
    fn parse_tool_accepts_claude_codex_and_mistral() {
        assert_eq!(parse_tool("claude"), Ok(ToolName::Claude));
        assert_eq!(parse_tool("codex"), Ok(ToolName::Codex));
        assert_eq!(parse_tool("mistral"), Ok(ToolName::Mistral));
    }

    /// The unknown-value error message must name every supported value, not
    /// just `claude` (issue #71 acceptance criterion) -- so a user who
    /// mistypes `--tool` sees the full closed set to choose from.
    #[test]
    fn parse_tool_rejects_an_unknown_value_and_lists_every_supported_one() {
        let error = parse_tool("aider").unwrap_err();
        assert!(error.contains("aider"), "{error:?}");
        assert!(error.contains("claude"), "{error:?}");
        assert!(error.contains("codex"), "{error:?}");
        assert!(error.contains("mistral"), "{error:?}");
    }
}
