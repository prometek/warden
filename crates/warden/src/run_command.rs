//! Runtime implementation for `warden run` command.

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;
use warden::agent_def::resolve_agent_definition;
use warden::db;
use warden::gate_trigger;
use warden::hook_config::{parse_repo_hooks, read_repo_hooks};
use warden::orchestrator::{
    self, ApprovalConfig, Orchestrator, RunConfig, RunExecutionContext, SandboxConfig,
};
use warden::policy_config::{parse_repo_policy, read_repo_policy};
use warden::policy_gate::PolicyGate;
use warden::tool_adapter::ToolName;
use warden_core::AgentRole;
use warden_sandbox::{LocalSandbox, Sandbox};

use crate::cli::{Isolation, IsolationConfig, TrustRepoAgents};

mod batch;

pub(crate) use batch::{run_batch, BatchCommand};

pub(crate) struct TuiLaunchConfig {
    pub(crate) tui_bin: PathBuf,
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
pub(crate) fn resolve_tui_binary(explicit: Option<PathBuf>) -> PathBuf {
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

/// Loads and validates this run's pipeline shape (issue #73):
/// `.warden/workflow.yaml` if present, else `Workflow::builtin_default()` --
/// the latter is what makes a run with no workflow file reproduce the
/// pre-issue-#73 pipeline exactly (strict retro-compat). Resolving each
/// step's own *agent* is a separate concern (`run`'s own step-resolution
/// loop, right after this call) -- it needs the adapter/user-config-dir/
/// trust-repo-agents context this function deliberately doesn't take, so
/// this one only ever fails on the workflow file itself being missing/
/// malformed.
///
/// **Issue #73 (trio-unification follow-up)**: no ordering restriction --
/// the built-in coder/reviewer/tester no longer need to appear first, or at
/// all. `Workflow::parse_yaml`'s own validation (non-empty steps, the first
/// step a plain pass-through, unique roles, no path-traversal-shaped
/// role/agent names) is the only shape this function itself enforces.
async fn load_workflow(repo: &std::path::Path) -> anyhow::Result<warden_core::Workflow> {
    let workflow_path = repo.join(WORKFLOW_FILE);
    match tokio::fs::read_to_string(&workflow_path).await {
        Ok(raw) => warden_core::Workflow::parse_yaml(&raw)
            .with_context(|| format!("invalid workflow file at {}", workflow_path.display())),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(warden_core::Workflow::builtin_default())
        }
        Err(source) => Err(source).with_context(|| {
            format!(
                "failed to read workflow file at {}",
                workflow_path.display()
            )
        }),
    }
}

/// Interactive human-validation wait point for `Decision::RequireApproval`
/// (issue #51/ADR-0016): prompts the operator on stderr and reads a `y`/`yes`
/// answer from stdin. `warden::policy_gate::ApprovalGate`'s own trait docs
/// say exactly why this concrete implementation lives here rather than in
/// the lib -- code-standards.md's "the lib emits tracing spans/events... it
/// never writes to stdout/stderr directly" is the same rule that already
/// keeps `Orchestrator::on_run_started`'s printing (`print_run_started_hint`,
/// below) out of the lib.
///
/// **Never installed when `--tui` is attached** (issue #51 review round 2,
/// finding A) -- see [`NoTuiApprovalGate`]'s own docs for why, and `run`'s
/// own call site for the `tui_launch.is_none()` gate that enforces it.
///
/// **Non-interactive session (no TTY on stdin or stderr)**: refuses to
/// prompt and denies outright, fail-closed -- there is no human to ask (or
/// nowhere visible to ask them: a prompt written to a redirected stderr is
/// invisible, yet `warden` would still block on stdin waiting for an answer
/// nobody can see to give), and blocking forever on a `read_line` that will
/// never receive input would hang the run rather than fail it cleanly. The
/// reason is printed to stderr (naming the action and the rule's own reason)
/// before returning `false` -- meaningful whenever *stderr itself* is still
/// a terminal (stdin redirected, stderr not), and otherwise at least
/// captured by whatever stderr was redirected to -- so a `RequireApproval`
/// in a non-interactive run (CI, a script) is never a silent, unexplained
/// failure. `PolicyGate::decide` additionally logs this refusal via
/// `tracing`, so it is never dropped entirely even when stderr is discarded.
///
/// **Concurrent interactive runs**: two `warden run` processes attached to
/// the *same* terminal (e.g. two shells sharing one tty via `screen`/`tmux`
/// panes misconfigured to the same device, or a backgrounded run sharing its
/// launching shell's tty with a foreground one) will interleave their
/// prompts and race for the next keystroke -- there is no locking across
/// processes. Batch mode (`warden run --intents-file ...`) is not subject to
/// this: `run_one_batch_intent` awaits each child inline, so batch intents
/// are strictly sequential, never concurrent, regardless of `--tui`.
struct TtyApprovalGate;

#[async_trait::async_trait]
impl warden::policy_gate::ApprovalGate for TtyApprovalGate {
    async fn approve(&self, request: warden::policy_gate::ApprovalRequest<'_>) -> bool {
        // Issue #51 review round 2, finding C: stdin alone is not enough --
        // `warden run ... 2> run.log` leaves stdin a real tty while stderr
        // (where the prompt is written) is redirected. Testing both means
        // the prompt is only ever attempted when there is somewhere visible
        // to show it *and* someone at the keyboard to answer it.
        if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
            eprintln!(
                "warden: run {} requires human approval for {} ({}) but this session is not \
                 fully interactive (stdin/stderr not both a terminal) -- refusing to prompt; \
                 denying",
                request.run_id, request.description, request.reason
            );
            return false;
        }

        eprint!(
            "warden: run {} requires approval for {} ({}) -- approve? [y/N] ",
            request.run_id, request.description, request.reason
        );
        // Issue #51 review round 2, finding D: a flush failure must not deny
        // silently -- every other branch of this method says why it denied,
        // and this one is no exception (code-standards.md: no catch-and-
        // ignore).
        if let Err(err) = std::io::stderr().flush() {
            eprintln!("warden: failed to write the approval prompt: {err} -- denying");
            return false;
        }

        let mut line = String::new();
        match BufReader::new(tokio::io::stdin())
            .read_line(&mut line)
            .await
        {
            Ok(0) => {
                eprintln!("warden: no input received (EOF) -- denying");
                false
            }
            Ok(_) => parse_approval_answer(&line),
            Err(err) => {
                eprintln!("warden: failed to read approval response: {err} -- denying");
                false
            }
        }
    }
}

/// Installed instead of [`TtyApprovalGate`] whenever `--tui` is attached
/// (issue #51 review round 2, finding A). `warden-tui` takes over the
/// shared terminal -- alternate screen plus raw mode
/// (`warden-tui`'s own `enable_raw_mode`/`EnterAlternateScreen`) -- the
/// moment it starts, which breaks every part of the interactive prompt at
/// once: the prompt itself would be written into a screen the TUI owns
/// (invisible, or corrupting its frame); raw mode delivers Enter as `\r`,
/// which [`AsyncBufReadExt::read_line`]'s `\n` scan never sees, so the read
/// never completes; and raw mode also clears `ISIG`, so not even Ctrl-C
/// reaches `warden` to break out. Installing [`TtyApprovalGate`] here
/// instead would net a run that hangs forever after spending the entire
/// agent budget, recoverable only by `kill` from another terminal -- worse
/// than simply failing the run. This denies
/// immediately instead, fail-closed, with a reason naming exactly why (no
/// interactive channel is available while `--tui` owns the terminal) and
/// what to do about it.
pub(crate) struct NoTuiApprovalGate;

#[async_trait::async_trait]
impl warden::policy_gate::ApprovalGate for NoTuiApprovalGate {
    async fn approve(&self, request: warden::policy_gate::ApprovalRequest<'_>) -> bool {
        eprintln!(
            "warden: run {} requires human approval for {} ({}) but --tui is attached -- the \
             terminal is owned by warden-tui, so no interactive approval prompt is available; \
             denying. Re-run without --tui to be prompted interactively, or adjust \
             .warden/policy.yaml so this action does not require approval.",
            request.run_id, request.description, request.reason
        );
        false
    }
}

/// Pure parsing of [`TtyApprovalGate`]'s stdin line -- split out from
/// [`TtyApprovalGate::approve`] so the accept/reject rule is unit-testable
/// without a real terminal (code-standards.md: tests déterministes, pas de
/// dépendance à un TTY). Only an explicit affirmative answer approves;
/// anything else -- blank, `"n"`, a typo -- denies, fail-closed.
///
/// **Not unit-tested here**: whether [`TtyApprovalGate::approve`] itself
/// prompts at all depends on `std::io::stdin().is_terminal()`, i.e. on the
/// *test process's own* ambient stdin -- true under this sandbox/CI (stdin
/// piped/redirected, never a real tty), but not guaranteed for a developer
/// running `cargo test` directly in an interactive shell with stdin
/// inherited. A test exercising that branch would then genuinely block on
/// `read_line`, waiting on a real terminal (code-standards.md: "pas de temps
/// réel non mocké" -- this is exactly that hazard) -- refactoring
/// `approve` to accept an injectable terminal-check purely to make that
/// safe was judged not worth it for a two-line guard clause, the same
/// call `resolve_tui_binary_falls_back_to_a_bare_name_when_no_sibling_binary_exists`'s
/// own doc comment already makes for `std::env::current_exe()`.
pub(crate) fn parse_approval_answer(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    repo: PathBuf,
    intent: String,
    branch: String,
    max_review_cycles: u32,
    max_test_cycles: u32,
    max_cycles: u32,
    quota_anticipation_threshold: f64,
    warden_home: Option<PathBuf>,
    adapter: ToolName,
    trust_repo_agents: TrustRepoAgents,
    evidence_tool: Option<warden_core::EvidenceTool>,
    evidence_store_in_repo: bool,
    gate: Option<orchestrator::GateConfig>,
    tui_launch: Option<TuiLaunchConfig>,
    isolation_config: IsolationConfig,
) -> anyhow::Result<()> {
    // Issue #25/ADR-0021: `--isolation worktree` (the default) gives the
    // agent subprocess the invoking OS user's full filesystem read rights
    // (write too, for `--tool claude` via its `Bash` grant, and for
    // `--tool mistral` since no adapter-side tool constraint or sandbox
    // flag exists at all -- see ADR-0021 §3bis for the per-adapter nuance
    // codex's own OS sandbox adds). Surfaced once, at
    // the top of every run (including each batch child, issue #72, since
    // each re-enters `run` as its own process), unconditionally and with no
    // suppression knob -- see ADR-0021 for why this is a direct stderr
    // write rather than `tracing::warn!`.
    if isolation_config.isolation == Isolation::Worktree {
        print_isolation_worktree_warning();
    }

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

    // Start quota recovery before any foreground repository preflight. The
    // foreground work is captured as a result below so every early `?` still
    // reaches the join point after this block.
    let quota_resume_pool = pool.clone();
    let quota_resume_handle =
        tokio::spawn(
            async move { orchestrator::resume_quota_suspended_runs(quota_resume_pool).await },
        );
    let foreground_result = run_foreground(
        pool,
        db_path,
        repo,
        intent,
        branch,
        max_review_cycles,
        max_test_cycles,
        max_cycles,
        quota_anticipation_threshold,
        warden_home,
        adapter,
        trust_repo_agents,
        evidence_tool,
        evidence_store_in_repo,
        gate,
        tui_launch,
        isolation_config,
        cancel,
    )
    .await;

    let quota_resume_result: anyhow::Result<Vec<String>> = match quota_resume_handle.await {
        Ok(result) => result.context("failed to resume runs awaiting a quota reset"),
        Err(error) => Err(error).context("quota-resume supervision task failed"),
    };
    match &quota_resume_result {
        Ok(resumed) => {
            for run_id in resumed {
                tracing::warn!(
                    run_id,
                    "resumed a run after its quota reset (crash recovery)"
                );
            }
        }
        Err(error) => {
            tracing::error!(%error, "supervised quota recovery failed");
        }
    }

    foreground_result?;
    quota_resume_result?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_foreground(
    pool: sqlx::SqlitePool,
    db_path: PathBuf,
    repo: PathBuf,
    intent: String,
    branch: String,
    max_review_cycles: u32,
    max_test_cycles: u32,
    max_cycles: u32,
    quota_anticipation_threshold: f64,
    warden_home: PathBuf,
    adapter: ToolName,
    trust_repo_agents: TrustRepoAgents,
    evidence_tool: Option<warden_core::EvidenceTool>,
    evidence_store_in_repo: bool,
    gate: Option<orchestrator::GateConfig>,
    tui_launch: Option<TuiLaunchConfig>,
    isolation_config: IsolationConfig,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
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
    // (`user_config_agents_dir`) is resolved from this process's real
    // environment (`XDG_CONFIG_HOME`/`HOME`) -- see
    // `agent_def::default_user_config_agents_dir`'s own docs for why that
    // env read lives here rather than inside `resolve_agent_definition`
    // itself. `warden_home` is passed alongside it (owner's ruling,
    // "escalated asymmetry"): a user-config source resolving under
    // `<warden_home>/worktrees/` -- a stale worktree from a crashed run --
    // must be degraded exactly like one resolving inside the repo itself.
    //
    // Issue #57: no longer resolved eagerly here. `default_user_config_agents_dir`
    // requires `XDG_CONFIG_HOME` or `HOME`; calling it unconditionally made
    // every `warden run` -- including one with `--warden-home` given
    // explicitly and a workflow with no "reviewer"/"tester" step at all --
    // abort in an environment with neither set (a systemd unit, a minimal
    // container, some CI runners), even though nothing in that run would
    // ever have used the result. This undid, ~100 lines later, the very
    // eager-evaluation fix made just above for `warden_home` (see that
    // `match`'s own comment). `resolve_lazy_user_config_agents_dir` (below)
    // resolves and memoizes it the moment a "reviewer"/"tester" step in the
    // loop below actually needs it, mirroring that same `match` pattern.
    let mut user_config_agents_dir: Option<PathBuf> = None;

    // Issue #73: `.warden/workflow.yaml`, if present, defines this run's
    // pipeline; its absence reproduces the pre-issue-#73 pipeline exactly
    // (`Workflow::builtin_default`) -- the strict retro-compat requirement
    // this whole feature is judged against. Loaded before any agent is
    // resolved, so a malformed workflow file fails fast.
    let workflow = load_workflow(&repo).await?;

    // Issue #73 (trio-unification follow-up): every step's agent is
    // resolved here, uniformly, in `workflow.steps` order -- no ordering
    // restriction on the workflow itself. A step literally named
    // `"coder"`/`"reviewer"`/`"tester"` still goes through the existing,
    // hardened, role-asymmetric resolution (`resolve_agent_definition`,
    // ADR-0026/issue #26) -- that trust model is inherent to what those
    // three names *mean* (independent judgement of the coder's own work),
    // not an artifact of their position in the pipeline. Any other role
    // goes through the simpler `resolve_custom_step_agent_definition`
    // (`.claude/agents/<agent>.md`, ADR-0013). `step_agents` ends up a flat
    // list the convergence loop indexes by position, never by name.
    let mut step_agents = Vec::with_capacity(workflow.steps.len());
    // Issue #26: `resolve_agent_definition` already `tracing::warn!`ed the
    // moment it actually read a repo-sourced reviewer/tester definition
    // (before this run, or its Event Bus, even exist) -- this just collects
    // which role(s) that happened for, so `run_convergence_loop` can also
    // publish a persisted `RunEvent::UntrustedAgentDefinitionUsed` for each,
    // once the run's own event log exists to carry it.
    let mut untrusted_repo_agent_definitions = Vec::new();
    for step in &workflow.steps {
        // Issue #79: a `type: hook` step has no agent definition to resolve
        // at all -- `Orchestrator::run_gated_step` never spawns anything for
        // it, so `step_agents` carries no entry for this step's position
        // (`ResolvedAgents::resolve`'s own contract: one entry per `type:
        // agent` step, in workflow order, not one per `workflow.steps`).
        if step.kind == warden_core::StepKind::Hook {
            continue;
        }
        let definition = match step.role.as_str() {
            "coder" => {
                // Issue #57: `resolve_agent_definition`'s `Coder` arm never
                // reads `user_config_agents_dir` at all -- documented on its
                // own doc comment and pinned by its own
                // `coder_resolution_ignores_the_user_config_dir_entirely`
                // unit test. An empty placeholder satisfies the shared
                // `&Path` parameter without resolving the real, possibly
                // env-dependent directory for a workflow whose only
                // built-in step is the coder itself.
                let (definition, _source) = resolve_agent_definition(
                    &repo,
                    AgentRole::Coder,
                    &adapter,
                    Path::new(""),
                    &warden_home,
                    trust_repo_agents.0,
                )
                .await?;
                definition
            }
            "reviewer" => {
                let user_config_dir =
                    resolve_lazy_user_config_agents_dir(&mut user_config_agents_dir)?;
                let (definition, source) = resolve_agent_definition(
                    &repo,
                    AgentRole::Reviewer,
                    &adapter,
                    user_config_dir,
                    &warden_home,
                    trust_repo_agents.0,
                )
                .await?;
                if let warden::agent_def::AgentDefinitionSource::UntrustedRepoOverride {
                    path,
                    canonical_path,
                } = source
                {
                    untrusted_repo_agent_definitions.push(
                        orchestrator::UntrustedRepoAgentDefinition {
                            role: AgentRole::Reviewer,
                            path,
                            canonical_path,
                        },
                    );
                }
                definition
            }
            "tester" => {
                let user_config_dir =
                    resolve_lazy_user_config_agents_dir(&mut user_config_agents_dir)?;
                let (definition, source) = resolve_agent_definition(
                    &repo,
                    AgentRole::Tester,
                    &adapter,
                    user_config_dir,
                    &warden_home,
                    trust_repo_agents.0,
                )
                .await?;
                if let warden::agent_def::AgentDefinitionSource::UntrustedRepoOverride {
                    path,
                    canonical_path,
                } = source
                {
                    untrusted_repo_agent_definitions.push(
                        orchestrator::UntrustedRepoAgentDefinition {
                            role: AgentRole::Tester,
                            path,
                            canonical_path,
                        },
                    );
                }
                definition
            }
            custom_role => {
                let agent_name = step.agent.as_deref().expect(
                    "Workflow::parse_yaml guarantees a type: agent step always carries a \
                     non-blank \"agent\"",
                );
                warden::agent_def::resolve_custom_step_agent_definition(
                    &repo,
                    custom_role,
                    agent_name,
                )
                .await
                .with_context(|| {
                    format!(
                        "failed to resolve the agent for custom workflow role {custom_role:?} \
                         (agent {agent_name:?})"
                    )
                })?
            }
        };
        step_agents.push(definition);
    }

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
        step_agents,
        evidence_tool,
        evidence_store_in_repo,
        gate,
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
    let sandbox_config = match isolation_config.isolation {
        Isolation::Worktree => SandboxConfig::Worktree,
        Isolation::Docker => SandboxConfig::Docker {
            image: isolation_config.image,
            claude_config_dir: default_claude_config_dir()?,
        },
    };
    let sandbox = sandbox_config.build(&config.repo_path);

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
    // Issue #49's agent-isolation choice is built once above and installed
    // here. The same `SandboxConfig` is persisted in a quota checkpoint so a
    // later startup cannot silently fall back from Docker to LocalSandbox.
    let orchestrator = Orchestrator::new(pool.clone())
        .with_sandbox(sandbox)
        .with_quota_anticipation_threshold(quota_anticipation_threshold);

    // Lifecycle hooks run on the HOST, never inside an agent's isolation
    // container: they are the operator's own infra prep (`docker compose up`,
    // `git pull`) against the repo as a whole, so they always go through a
    // LocalSandbox regardless of `--isolation`. Absent `.warden/hooks.toml` =>
    // empty registry (dispatch stays a no-op). See `warden::hook_config` for
    // the trust model: a repo's hook commands are honoured by default,
    // consistent with its `.warden/agents/coder.md`.
    // Issue #51/ADR-0016: `.warden/policy.yaml` resolved once, shared between
    // this run's `.warden/hooks.toml` commands (each `CommandHook`'s own
    // shell-command decision point) and the orchestrator's own `git_push`
    // decision point (`gate_tail::drive_post_convergence_tail`) -- a single
    // rule set governs both. Absent file -> `PolicyGate::empty` (no-op,
    // strict parity with pre-issue-#51 behaviour).
    //
    // Issue #51 review round 2, finding A: the human-validation wait point a
    // `RequireApproval` decision suspends on is `TtyApprovalGate` only when
    // no `--tui` is attached -- `warden-tui` owns the shared terminal (raw
    // mode, alternate screen) the instant it starts, which would turn an
    // interactive prompt into a permanent, unrecoverable-by-Ctrl-C hang
    // (see `NoTuiApprovalGate`'s own docs). `tui_launch` already reflects
    // `--tui` for this invocation, batch child or not (`run_one_batch_intent`
    // re-parses argv fresh in the child, forwarding the flag verbatim).
    let policy_yaml =
        read_repo_policy(&config.repo_path).context("failed to load .warden/policy.yaml")?;
    let policy_rules = parse_repo_policy(&config.repo_path, policy_yaml.as_deref())
        .context("failed to parse .warden/policy.yaml")?;
    let approval = if tui_launch.is_none() {
        ApprovalConfig::InteractiveTty
    } else {
        ApprovalConfig::FailClosed
    };
    let approval_gate: Arc<dyn warden::policy_gate::ApprovalGate> = if tui_launch.is_none() {
        Arc::new(TtyApprovalGate)
    } else {
        Arc::new(NoTuiApprovalGate)
    };
    let policy_gate = Arc::new(
        PolicyGate::new(warden_policy::Evaluator::new(policy_rules))
            .with_approval_gate(approval_gate),
    );

    let hook_sandbox: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new());
    let hooks_toml =
        read_repo_hooks(&config.repo_path).context("failed to load .warden/hooks.toml")?;
    let hooks = parse_repo_hooks(
        &config.repo_path,
        hooks_toml.as_deref(),
        hook_sandbox,
        Arc::clone(&policy_gate),
    )
    .context("failed to parse .warden/hooks.toml")?;
    let execution_context = RunExecutionContext {
        tool: adapter,
        sandbox: sandbox_config,
        hooks_toml,
        policy_yaml,
        approval,
    };

    let orchestrator = orchestrator
        .with_hooks(hooks)
        .with_policy_gate(policy_gate)
        .with_run_execution_context(execution_context)
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
    // Issue #72 review, MEDIUM 1: a second, dedicated machine-readable line,
    // deliberately keyed off `RunState::as_str()` -- warden-core's own
    // documented, migration-guarded stable string form (`state.rs`'s own
    // docs: "Never change existing variants' strings without a migration")
    // -- rather than `RunState`'s `Debug` output above, which carries no
    // such guarantee. `warden::batch`'s success classification for a batch
    // child (`warden::batch::parse_outcome_line`/`is_converged_state`) is
    // based on *this* line, never the human-readable `"finished: ..."` one,
    // so a cosmetic `Debug` reformat can never silently misclassify a
    // batch's outcome. Printed unconditionally (every run, not only a batch
    // child) rather than only under `--intent`/batch mode, so this is one
    // single, always-present contract instead of a conditional one a caller
    // would need to know to opt into.
    print_stdout_line_or_log(&format!("run {run_id} outcome: {}", final_state.as_str()));

    Ok(())
}

/// Issue #72's batch mode: runs `intents` sequentially, each as its own
/// fresh `warden run --intent <intent>` **subprocess** of this same binary
/// (`std::env::current_exe`), never in this process. This is the strong
/// isolation this issue defaults to: a brand new OS process gets a brand new
/// `Orchestrator`, a brand new `run_id`, and its own
/// `<warden_home>/worktrees/<run_id>/` tree, so there is no in-memory state
/// to carry over between intents by construction.
///
/// **Teardown (issue #72 review, MEDIUM 2): guaranteed on a clean child
/// exit, best-effort otherwise.** When a child's own convergence loop
/// returns normally (converged, exhausted its budget, or a handled failure),
/// this crate's existing, unchanged agent-subprocess (`kill_on_drop`) and
/// worktree teardown already guarantees its agents are gone and its
/// worktree is removed before this fn ever spawns the next intent's child --
/// exactly like any single-intent run. A child that instead dies uncleanly
/// (`SIGKILL`/OOM/abort) never runs that teardown at all (no `Drop` ever
/// fires) and can leave an orphaned agent process and/or worktree behind;
/// this fn does not detect that case itself, but reclaims it the same way
/// every `warden run` startup already does for exactly this scenario --
/// see the `orchestrator::recover_crashed_runs` call below, run once after
/// every intent (not left to the *next* intent's own incidental startup
/// call, which would never fire at all for the batch's last intent, or one
/// stopped at via `fail_fast`/cancellation).
///
/// Every flag here mirrors the one `--intent`/`--intents-file`/`--fail-fast`
/// case of [`Commands::Run`] (this fn's own caller is the only place that
/// destructures those three out before reaching here) -- forwarded to each
/// child verbatim via [`warden::batch::build_single_intent_args`], so a
/// batch child's own behaviour (gate tail, `--isolation docker`, evidence
/// capture, ...) is identical to running that one intent directly.
///
/// A non-converged intent (exhausted its cycle budget, or its own
/// subprocess failed outright) does not stop the batch -- the next intent
/// still gets its own clean-slate child -- unless `fail_fast` is set, in
/// which case every intent after the first non-converged one is recorded as
/// `Skipped` without ever being attempted. A Ctrl-C during the batch (issue
/// #72 review, LOW 1) is handled the same way: the in-flight child is left
/// to finish (it gets the same signal, in the same foreground process
/// group, and cancels itself exactly like a plain `warden run` would), then
/// every intent after it is recorded `Skipped` rather than started. Once
/// every intent has either run or been skipped, the batch summary is
/// printed and this returns `Err` iff at least one intent did not converge
/// (issue #72: "final report listing each intent's result").
#[allow(clippy::too_many_arguments)]
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
pub(crate) fn should_wait_for_spawned_tui(stdout_is_terminal: bool) -> bool {
    stdout_is_terminal
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

/// Issue #25/ADR-0021: prints the `--isolation worktree` filesystem-boundary
/// warning straight to stderr through a locked handle, deliberately
/// bypassing `tracing::warn!` -- `init_tracing`'s `EnvFilter::
/// try_from_default_env()` lets any `RUST_LOG` value replace the
/// `warden=warn` default wholesale, which would let a common dev-shell env
/// var silently suppress a security notice this exists to make
/// unsuppressible (see ADR-0021 for the alternatives considered, including
/// why this stays unconditional -- no opt-out env var, no once-per-home
/// suppression -- and prints once per batch child, issue #72).
///
/// Same broken-pipe tolerance as `print_stdout_line_or_log` above, for the
/// same reason: this is an advisory line, not something a run's own
/// correctness depends on reaching a terminal.
fn print_isolation_worktree_warning() {
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    if let Err(error) = writeln!(
        handle,
        "warden: warning: --isolation worktree (default): the agent runs directly on this \
         host, as this OS user. env_clear() bounds only environment variables -- it never \
         sandboxes the filesystem: ~/.ssh, ~/.aws, ~/.config/gh, or any other file this user \
         can read remains reachable by absolute path, and writable too depending on the agent \
         tool's own permissions (see docs/adr/ADR-0021 for the --tool breakdown). Use \
         --isolation docker for a real filesystem boundary."
    ) {
        if error.kind() != std::io::ErrorKind::BrokenPipe {
            tracing::warn!(%error, "failed to print isolation warning to stderr");
        }
    }
}

/// Resolves the reviewer/tester's trusted user-config agents directory
/// (`warden::agent_def::default_user_config_agents_dir`) on first use and
/// memoizes it in `cache` (issue #57): the `for step in &workflow.steps`
/// loop above calls this only from the "reviewer"/"tester" arms, so a
/// workflow with neither step never touches `XDG_CONFIG_HOME`/`HOME` at
/// all, and a workflow with both only reads the env once. Mirrors the
/// `match` already used for `--warden-home` a few lines above `run`'s own
/// call site (same "an `Option::unwrap_or`-shaped eager evaluation would
/// silently require an env var this run may not need" review finding).
///
/// Returns a borrow of the cached `PathBuf` rather than a clone (issue #57
/// review, finding 3: code-standards.md's "chaque clone justifié") -- no
/// caller needs an owned copy, and `cache` outlives every borrow this
/// returns (each call site's borrow ends with that loop iteration, well
/// before `cache` itself is next mutably borrowed on a later iteration).
fn resolve_lazy_user_config_agents_dir(cache: &mut Option<PathBuf>) -> anyhow::Result<&Path> {
    match cache {
        Some(dir) => Ok(dir.as_path()),
        None => Ok(cache
            .insert(warden::agent_def::default_user_config_agents_dir()?)
            .as_path()),
    }
}

pub(crate) fn default_warden_home() -> anyhow::Result<PathBuf> {
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
