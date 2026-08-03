//! Runtime implementation for `warden run` command.

use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
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
use warden_sandbox::{DockerEgressConfig, DockerRunOptions, LocalSandbox, Sandbox};

use crate::cli::{Isolation, IsolationConfig};

mod batch;

pub(crate) use batch::{run_batch, BatchCommand};

pub(crate) struct TuiLaunchConfig {
    pub(crate) tui_bin: PathBuf,
}

/// Resolves the `warden-tui` binary `--tui` spawns.
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

const USER_WORKFLOW_FILE: &str = "workflow.yaml";

struct LoadedWorkflow {
    workflow: warden_core::Workflow,
    definitions_root: PathBuf,
    repository_agent_definitions: bool,
}

async fn parse_workflow(
    workflow_path: PathBuf,
    definitions_root: PathBuf,
    repository_agent_definitions: bool,
) -> anyhow::Result<LoadedWorkflow> {
    match tokio::fs::read_to_string(&workflow_path).await {
        Ok(raw) => Ok(LoadedWorkflow {
            workflow: warden_core::Workflow::parse_yaml(&raw)
                .with_context(|| format!("invalid workflow file at {}", workflow_path.display()))?,
            definitions_root,
            repository_agent_definitions,
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            Err(source).with_context(|| {
                format!(
                    "workflow file does not exist at {}",
                    workflow_path.display()
                )
            })
        }
        Err(source) => Err(source).with_context(|| {
            format!(
                "failed to read workflow file at {}",
                workflow_path.display()
            )
        }),
    }
}

async fn load_workflow(
    repo: &std::path::Path,
    warden_home: &std::path::Path,
) -> anyhow::Result<LoadedWorkflow> {
    let repository_root = repo.join(".warden");
    let repository_workflow = repository_root.join("workflow.yaml");
    match parse_workflow(repository_workflow.clone(), repository_root, true).await {
        Ok(workflow) => Ok(workflow),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|source| source.kind() == std::io::ErrorKind::NotFound) =>
        {
            let user_workflow = warden_home.join(USER_WORKFLOW_FILE);
            match parse_workflow(user_workflow.clone(), warden_home.to_path_buf(), false).await {
                Ok(workflow) => Ok(workflow),
                Err(user_error) if user_error.downcast_ref::<std::io::Error>().is_some_and(|source| {
                    source.kind() == std::io::ErrorKind::NotFound
                }) => bail!(
                    "workflow file is required at {} or {} (copy an example and define explicit entry/transitions)",
                    repository_workflow.display(),
                    user_workflow.display(),
                ),
                Err(user_error) => Err(user_error),
            }
        }
        Err(error) => Err(error),
    }
}

/// Interactive human-validation wait point for `Decision::RequireApproval`: prompts the operator on
/// stderr and reads a `y`/`yes` answer from stdin.
struct TtyApprovalGate;

#[async_trait::async_trait]
impl warden::policy_gate::ApprovalGate for TtyApprovalGate {
    async fn approve(&self, request: warden::policy_gate::ApprovalRequest<'_>) -> bool {
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

/// Installed instead of [`TtyApprovalGate`] whenever `--tui` is attached.
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

/// Pure parsing of [`TtyApprovalGate`]'s stdin line.
pub(crate) fn parse_approval_answer(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    repo: PathBuf,
    intent: String,
    branch: String,
    max_cycles: u32,
    quota_anticipation_threshold: f64,
    warden_home: Option<PathBuf>,
    adapter: ToolName,
    evidence_tool: Option<warden_core::EvidenceTool>,
    evidence_store_in_repo: bool,
    gate: Option<orchestrator::GateConfig>,
    tui_launch: Option<TuiLaunchConfig>,
    isolation_config: IsolationConfig,
) -> anyhow::Result<()> {
    if isolation_config.isolation == Isolation::Worktree {
        print_isolation_worktree_warning();
    }

    let warden_home = match warden_home {
        Some(warden_home) => warden_home,
        None => default_warden_home()?,
    };
    let db_path = warden_home.join("state.db");
    let pool = db::connect(&db_path)
        .await
        .context("failed to open Warden's SQLite database")?;

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

    // Start quota recovery before any foreground repository preflight.
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
        max_cycles,
        quota_anticipation_threshold,
        warden_home,
        adapter,
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
    max_cycles: u32,
    quota_anticipation_threshold: f64,
    warden_home: PathBuf,
    adapter: ToolName,
    evidence_tool: Option<warden_core::EvidenceTool>,
    evidence_store_in_repo: bool,
    gate: Option<orchestrator::GateConfig>,
    tui_launch: Option<TuiLaunchConfig>,
    isolation_config: IsolationConfig,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
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

    let loaded_workflow = load_workflow(&repo, &warden_home).await?;
    let workflow = loaded_workflow.workflow;

    let mut step_agents = Vec::with_capacity(workflow.steps.len());
    for step in &workflow.steps {
        if step.kind == warden_core::StepKind::Command {
            continue;
        }
        let agent_name = step.agent.as_deref().expect("validated agent step");
        let definition = resolve_agent_definition(
            &loaded_workflow.definitions_root,
            step.role.as_str(),
            agent_name,
        )
        .await
        .with_context(|| {
            format!(
                "failed to resolve workflow step {:?} agent {:?}",
                step.role.as_str(),
                agent_name
            )
        })?;
        step_agents.push(definition);
    }

    let attach_warden_home =
        std::path::absolute(&warden_home).unwrap_or_else(|_| warden_home.clone());

    let attach_warden_home_quoted =
        shlex::try_quote(attach_warden_home.to_str().with_context(|| {
            format!(
                "--warden-home ({}) is not valid UTF-8; cannot render a copy-pasteable \
                 `warden-tui attach` command",
                attach_warden_home.display()
            )
        })?)
        .map(|quoted| quoted.into_owned())
        .context("--warden-home cannot be shell-quoted (embedded NUL byte)")?;

    let config = RunConfig {
        repo_path: repo,
        warden_home,
        branch,
        intent,
        max_cycles,
        workflow,
        step_agents,
        repository_agent_definitions: loaded_workflow.repository_agent_definitions,
        evidence_tool,
        evidence_store_in_repo,
        gate,
    };

    let sandbox_config = match isolation_config.isolation {
        Isolation::Worktree => {
            if isolation_config.cpus.is_some()
                || isolation_config.memory.is_some()
                || isolation_config.network.is_some()
                || isolation_config.egress_proxy.is_some()
            {
                bail!("--docker-* options require --isolation docker");
            }
            SandboxConfig::Worktree
        }
        Isolation::Docker => SandboxConfig::Docker {
            image: isolation_config.image,
            claude_config_dir: default_claude_config_dir()?,
            run_options: DockerRunOptions {
                cpus: isolation_config.cpus,
                memory: isolation_config.memory,
                egress: match (isolation_config.network, isolation_config.egress_proxy) {
                    (None, None) => None,
                    (Some(network), Some(proxy)) => Some(DockerEgressConfig { network, proxy }),
                    _ => bail!(
                        "Docker egress requires both --docker-network and --docker-egress-proxy"
                    ),
                },
            },
        },
    };
    let sandbox = sandbox_config.build(&config.repo_path);

    let cancel_on_tui_exit = cancel.clone();

    let tui_watcher: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let tui_watcher_setter = tui_watcher.clone();

    let tui_spawn_error: std::sync::Arc<std::sync::Mutex<Option<warden::error::ProcessError>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let tui_spawn_error_setter = tui_spawn_error.clone();

    let orchestrator = Orchestrator::new(pool.clone())
        .with_sandbox(sandbox)
        .with_quota_anticipation_threshold(quota_anticipation_threshold);

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

            // `--tui` spawns `warden-tui attach` as a separate process, in the foreground on this
            // launch terminal, once the run_id it needs actually exists.
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

    if should_wait_for_spawned_tui(std::io::stdout().is_terminal()) {
        let tui_watcher_handle = tui_watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = tui_watcher_handle {
            let _ = handle.await;
        }
    }

    if let Some(spawn_error) = tui_spawn_error
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        return Err(spawn_error).context("failed to spawn warden-tui for --tui; aborted the run");
    }

    let (run_id, final_state) = convergence_result.context("convergence loop failed")?;

    tracing::info!(run_id, ?final_state, "run finished");
    print_stdout_line_or_log(&format!("run {run_id} finished: {final_state:?}"));
    print_stdout_line_or_log(&format!("run {run_id} outcome: {}", final_state.as_str()));

    Ok(())
}

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

pub(crate) fn should_wait_for_spawned_tui(stdout_is_terminal: bool) -> bool {
    stdout_is_terminal
}

/// Prints the two `warden run`-start lines through a locked stdout handle instead of `println!`.
fn print_run_started_hint(run_id: &str, quoted_warden_home: &str) {
    print_stdout_line_or_log(&format!("run {run_id} started"));
    print_stdout_line_or_log(&format!(
        "attach: warden-tui attach --run-id {run_id} --warden-home {quoted_warden_home}"
    ));
}

/// Writes `line` + a newline to stdout through a locked handle, in place of `println!`, which
/// panics outright if stdout is closed.
fn print_stdout_line_or_log(line: &str) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if let Err(error) = writeln!(handle, "{line}") {
        if error.kind() != std::io::ErrorKind::BrokenPipe {
            tracing::warn!(%error, "failed to print to stdout");
        }
    }
}

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

pub(crate) fn default_warden_home() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set; pass --warden-home explicitly")?;
    if home.trim().is_empty() {
        bail!("HOME is empty; pass --warden-home explicitly");
    }
    Ok(PathBuf::from(home).join(".warden"))
}

fn default_claude_config_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME")
        .context("HOME is not set; cannot resolve ~/.claude for --isolation docker")?;
    if home.trim().is_empty() {
        bail!("HOME is empty; cannot resolve ~/.claude for --isolation docker");
    }
    Ok(PathBuf::from(home).join(".claude"))
}
