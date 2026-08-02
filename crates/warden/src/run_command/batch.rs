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
    pub(crate) max_cycles: u32,
    pub(crate) quota_anticipation_threshold: f64,
    pub(crate) warden_home: Option<PathBuf>,
    pub(crate) verbose: u8,
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
    pub(crate) docker_cpus: Option<String>,
    pub(crate) docker_memory: Option<String>,
    pub(crate) docker_network: Option<String>,
    pub(crate) docker_egress_proxy: Option<String>,
}

pub(crate) async fn run_batch(command: BatchCommand<'_>) -> anyhow::Result<()> {
    let BatchCommand {
        repo,
        intents,
        fail_fast,
        branch,
        max_cycles,
        quota_anticipation_threshold,
        warden_home,
        verbose,
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
        docker_cpus,
        docker_memory,
        docker_network,
        docker_egress_proxy,
    } = command;
    let warden_home = match warden_home {
        Some(warden_home) => warden_home,
        None => default_warden_home()?,
    };
    let current_exe = std::env::current_exe().context(
        "failed to resolve the path to the running warden binary (needed to spawn batch children)",
    )?;

    let pool = warden::db::connect(&warden_home.join("state.db"))
        .await
        .context("failed to open Warden's SQLite database for batch crash recovery")?;

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
        max_cycles,
        quota_anticipation_threshold,
        warden_home: warden_home_str,
        tool,
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
        docker_cpus: docker_cpus.as_deref(),
        docker_memory: docker_memory.as_deref(),
        docker_network: docker_network.as_deref(),
        docker_egress_proxy: docker_egress_proxy.as_deref(),
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

/// Converts a `Path` into the `&str` a batch child's argv needs, naming `flag_name` in the error so
/// a non-UTF-8 `--repo`/`--warden-home`/...
fn path_arg<'a>(path: &'a Path, flag_name: &str) -> anyhow::Result<&'a str> {
    path.to_str().with_context(|| {
        format!(
            "{flag_name} ({}) is not valid UTF-8; cannot forward it to a batch child process",
            path.display()
        )
    })
}

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
    let mut finished_display: Option<(String, String)> = None;
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
            // Prefers the `Debug`-form label for the report's own text when available (nicer to
            // read), falling back to the stable form itself -- purely cosmetic, never the
            // classification.
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
