//! Multi-intent subprocess execution.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context};
use tokio::io::{AsyncBufReadExt, BufReader};
use warden::orchestrator;

use super::{default_warden_home, print_stdout_line_or_log};

pub(crate) struct BatchCommand<'a> {
    pub(crate) repo: PathBuf,
    pub(crate) intents: Vec<String>,
    pub(crate) fail_fast: bool,
    pub(crate) branch: String,
    pub(crate) max_review_cycles: u32,
    pub(crate) max_test_cycles: u32,
    pub(crate) max_cycles: u32,
    pub(crate) quota_anticipation_threshold: f64,
    pub(crate) warden_home: Option<PathBuf>,
    pub(crate) verbose: u8,
    pub(crate) trust_repo_agents: bool,
    pub(crate) evidence_tool: Option<warden_core::EvidenceTool>,
    pub(crate) evidence_store_in_repo: bool,
    pub(crate) gate_bare_repo: Option<PathBuf>,
    pub(crate) gate_gated_bin: Option<PathBuf>,
    pub(crate) gate_repo_slug: Option<String>,
    pub(crate) gate_poll_interval_secs: u64,
    pub(crate) gate_inactivity_timeout_secs: u64,
    pub(crate) tui: bool,
    pub(crate) tui_bin: Option<PathBuf>,
    pub(crate) tool: &'a str,
    pub(crate) isolation: &'a str,
    pub(crate) isolation_image: String,
}

pub(crate) async fn run_batch(command: BatchCommand<'_>) -> anyhow::Result<()> {
    let BatchCommand {
        repo,
        intents,
        fail_fast,
        branch,
        max_review_cycles,
        max_test_cycles,
        max_cycles,
        quota_anticipation_threshold,
        warden_home,
        verbose,
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
        tool,
        isolation,
        isolation_image,
    } = command;
    // Resolved once here (not once per child), so every intent in this batch
    // shares the exact same `--warden-home` regardless of which point in
    // time `HOME` is read at -- same rationale as `run`'s own resolution.
    let warden_home = match warden_home {
        Some(warden_home) => warden_home,
        None => default_warden_home()?,
    };
    let current_exe = std::env::current_exe().context(
        "failed to resolve the path to the running warden binary (needed to spawn batch children)",
    )?;

    // Issue #72 review, MEDIUM 2: opened once here (not per intent) so the
    // crash-recovery call after each intent below reuses the same pool --
    // this is the exact same `<warden_home>/state.db` every batch child
    // opens for itself, so this parent process reads/writes nothing a
    // concurrently-running child wouldn't already expect another `warden`
    // process to touch (ADR-0004's SQLite is already multi-writer-safe via
    // WAL, and no child is alive when this call runs -- it only runs
    // between children, once each has already exited).
    let pool = warden::db::connect(&warden_home.join("state.db"))
        .await
        .context("failed to open Warden's SQLite database for batch crash recovery")?;

    // Issue #72 review, LOW 1: without this, an unhandled Ctrl-C would kill
    // this process outright (default `SIGINT` disposition), abandoning the
    // loop below without ever printing the batch summary -- even though the
    // in-flight child (same foreground process group) already receives and
    // handles that same signal itself, exactly like a plain `warden run`
    // would. This only arms a flag; it never touches the in-flight child.
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancelled_setter = cancelled.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::warn!(
                "received Ctrl-C during batch mode; finishing the in-flight intent, then \
                 skipping the rest and printing the summary"
            );
            cancelled_setter.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let repo_str = path_arg(&repo, "--repo")?;
    let warden_home_str = path_arg(&warden_home, "--warden-home")?;
    let gate_bare_repo_str = gate_bare_repo
        .as_deref()
        .map(|path| path_arg(path, "--gate-bare-repo"))
        .transpose()?;
    let gate_gated_bin_str = gate_gated_bin
        .as_deref()
        .map(|path| path_arg(path, "--gate-gated-bin"))
        .transpose()?;
    let tui_bin_str = tui_bin
        .as_deref()
        .map(|path| path_arg(path, "--tui-bin"))
        .transpose()?;
    let evidence_tool_str = evidence_tool.map(warden_core::EvidenceTool::as_str);

    let single_intent_args = warden::batch::SingleIntentArgs {
        repo: repo_str,
        branch: &branch,
        max_review_cycles,
        max_test_cycles,
        max_cycles,
        quota_anticipation_threshold,
        warden_home: warden_home_str,
        tool,
        trust_repo_agents,
        evidence_tool: evidence_tool_str,
        evidence_store_in_repo,
        gate_bare_repo: gate_bare_repo_str,
        gate_gated_bin: gate_gated_bin_str,
        gate_repo_slug: gate_repo_slug.as_deref(),
        gate_poll_interval_secs,
        gate_inactivity_timeout_secs,
        tui,
        tui_bin: tui_bin_str,
        isolation,
        isolation_image: &isolation_image,
        verbose,
    };

    let total = intents.len();
    let mut reports: Vec<warden::batch::IntentReport> = Vec::with_capacity(total);
    let mut stop_remaining = false;
    let mut skip_reason = String::new();

    for (index, intent) in intents.iter().enumerate() {
        if stop_remaining {
            print_stdout_line_or_log(&format!(
                "batch: skipping intent {}/{total} ({skip_reason}): {intent:?}",
                index + 1
            ));
            reports.push(warden::batch::IntentReport {
                intent: intent.clone(),
                run_id: None,
                status: warden::batch::IntentStatus::Skipped {
                    reason: skip_reason.clone(),
                },
            });
            continue;
        }

        print_stdout_line_or_log(&format!(
            "batch: starting intent {}/{total}: {intent:?}",
            index + 1
        ));

        let child_args = warden::batch::build_single_intent_args(&single_intent_args, intent);
        let report = run_one_batch_intent(&current_exe, &child_args, intent).await?;

        // Issue #72 review, MEDIUM 2: reclaims this intent's own orphaned
        // agent process(es)/worktree if its child crashed uncleanly --
        // best-effort, exactly like every `warden run` startup already does
        // (see this fn's own docs above). Run unconditionally, whether the
        // intent converged or not: a converging child already tore down
        // after itself, so this is a cheap no-op for it.
        match orchestrator::recover_crashed_runs(&pool).await {
            Ok(recovered) => {
                for recovered_run_id in &recovered {
                    tracing::warn!(
                        run_id = recovered_run_id,
                        "batch: run recovered as Failed after its child intent exited (crash \
                         recovery)"
                    );
                }
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    "batch: crash recovery after an intent's child failed"
                );
            }
        }

        if let Some(reason) = warden::batch::stop_reason(
            report.status.is_success(),
            fail_fast,
            cancelled.load(std::sync::atomic::Ordering::SeqCst),
        ) {
            stop_remaining = true;
            skip_reason = reason;
        }
        reports.push(report);
    }

    print_stdout_line_or_log("");
    print_stdout_line_or_log(&warden::batch::summarize(&reports));

    if warden::batch::batch_failed(&reports) {
        let converged = reports
            .iter()
            .filter(|report| report.status.is_success())
            .count();
        bail!("batch finished with {converged}/{total} intent(s) converged (see summary above)");
    }

    Ok(())
}

/// Converts a `Path` into the `&str` a batch child's argv needs (issue #72),
/// naming `flag_name` in the error so a non-UTF-8 `--repo`/`--warden-home`/...
/// fails clearly rather than being silently mangled via `Path::display()`
/// (code-standards.md: "no silent fallback") -- the same shape
/// `attach_warden_home_quoted` above already uses for the analogous
/// `--warden-home` case.
fn path_arg<'a>(path: &'a Path, flag_name: &str) -> anyhow::Result<&'a str> {
    path.to_str().with_context(|| {
        format!(
            "{flag_name} ({}) is not valid UTF-8; cannot forward it to a batch child process",
            path.display()
        )
    })
}

/// Runs one batch intent's child (issue #72): spawns `current_exe` with
/// `child_args`, relays its stdout live (through the same
/// [`print_stdout_line_or_log`] every other run output goes through) while
/// parsing it for the `"run <id> started"`, `"run <id> finished: <Debug>"`,
/// and `"run <id> outcome: <as_str()>"` lines [`run`] itself always prints,
/// then classifies the outcome once the child exits.
///
/// Issue #72 review, MEDIUM 1: classification (converged or not) is decided
/// **only** from the `"... outcome: ..."` line -- `RunState::as_str()`'s
/// stable, migration-guarded string form -- never from the human-readable
/// `"... finished: <Debug>"` line, which carries no such stability
/// guarantee. The `Debug` text is still captured, purely to give
/// [`warden::batch::summarize`] a nicer label than the `snake_case` stable
/// form; a child that (for whatever reason) prints one line but not the
/// other is treated as untrustworthy either way (see the final `match`
/// below) -- this batch never classifies an intent from a partial read.
///
/// Returns `Err` only for an infrastructure failure this batch cannot
/// meaningfully continue past (the child failed to even spawn, or waiting on
/// it failed) -- a *child* that ran and reported a non-converged outcome, or
/// exited non-zero, is instead reported as a normal (non-`Err`)
/// [`warden::batch::IntentStatus::SubprocessError`]/`NotConverged`, so the
/// batch's own continue-by-default policy can act on it.
async fn run_one_batch_intent(
    current_exe: &Path,
    child_args: &[String],
    intent: &str,
) -> anyhow::Result<warden::batch::IntentReport> {
    let mut command = tokio::process::Command::new(current_exe);
    command
        .args(child_args)
        .stdout(std::process::Stdio::piped());

    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to spawn batch child `{} {}`",
            current_exe.display(),
            child_args.join(" ")
        )
    })?;

    let stdout = child
        .stdout
        .take()
        .context("batch child stdout was not piped (internal bug)")?;
    let mut lines = BufReader::new(stdout).lines();

    let mut run_id: Option<String> = None;
    // Display-only (issue #72 review, MEDIUM 1): the human-readable
    // `RunState` `Debug` text, used purely for `summarize`'s label.
    let mut finished_display: Option<(String, String)> = None;
    // The classification source of truth: `RunState::as_str()`'s stable form.
    let mut outcome: Option<(String, String)> = None;

    while let Some(line) = lines
        .next_line()
        .await
        .context("failed to read batch child stdout")?
    {
        print_stdout_line_or_log(&line);
        if run_id.is_none() {
            if let Some(started_id) = warden::batch::parse_started_line(&line) {
                run_id = Some(started_id.to_string());
            }
        }
        if let Some((finished_id, debug_state)) = warden::batch::parse_finished_line(&line) {
            finished_display = Some((finished_id.to_string(), debug_state.to_string()));
        }
        if let Some((outcome_id, stable_state)) = warden::batch::parse_outcome_line(&line) {
            outcome = Some((outcome_id.to_string(), stable_state.to_string()));
        }
    }

    let status = child
        .wait()
        .await
        .with_context(|| format!("failed to wait for batch child (intent {intent:?})"))?;

    if !status.success() {
        return Ok(warden::batch::IntentReport {
            intent: intent.to_string(),
            run_id: run_id.or_else(|| outcome.as_ref().map(|(id, _)| id.clone())),
            status: warden::batch::IntentStatus::SubprocessError {
                reason: format!("warden run exited with status {status}"),
            },
        });
    }

    match outcome {
        Some((outcome_run_id, stable_state)) => {
            // Prefers the `Debug`-form label for the report's own text when
            // available (nicer to read), falling back to the stable form
            // itself -- purely cosmetic, never the classification.
            let final_state = finished_display
                .map(|(_, debug_state)| debug_state)
                .unwrap_or_else(|| stable_state.clone());
            let status = if warden::batch::is_converged_state(&stable_state) {
                warden::batch::IntentStatus::Converged { final_state }
            } else {
                warden::batch::IntentStatus::NotConverged { final_state }
            };
            Ok(warden::batch::IntentReport {
                intent: intent.to_string(),
                run_id: Some(outcome_run_id),
                status,
            })
        }
        None => Ok(warden::batch::IntentReport {
            intent: intent.to_string(),
            run_id,
            status: warden::batch::IntentStatus::SubprocessError {
                reason: "child exited successfully but printed no parseable \"outcome:\" line"
                    .to_string(),
            },
        }),
    }
}
