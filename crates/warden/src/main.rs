//! `warden` binary: CLI parsing + dispatch only.

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::Parser;
use warden::orchestrator;
#[cfg(test)]
use warden::tool_adapter::ToolName;

mod cli;
mod run_command;

use cli::{isolation_as_str, tool_as_str, Cli, Commands, IsolationConfig, TrustRepoAgents};
#[cfg(test)]
use cli::{parse_quota_anticipation_threshold, parse_tool};
#[cfg(test)]
use run_command::{parse_approval_answer, should_wait_for_spawned_tui, NoTuiApprovalGate};
use run_command::{resolve_tui_binary, run, run_batch, BatchCommand, TuiLaunchConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let verbose = cli.verbose;

    match cli.command {
        Commands::Run {
            repo,
            intent,
            intents_file,
            fail_fast,
            branch,
            max_review_cycles,
            max_test_cycles,
            max_cycles,
            quota_anticipation_threshold,
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
            let mut intents = Vec::new();
            let mut empty_intents_file: Option<&PathBuf> = None;
            if let Some(intents_file_path) = &intents_file {
                let file_intents = warden::batch::read_intents_file(intents_file_path)
                    .with_context(|| {
                        format!(
                            "failed to read --intents-file {}",
                            intents_file_path.display()
                        )
                    })?;
                if file_intents.is_empty() {
                    empty_intents_file = Some(intents_file_path);
                }
                intents.extend(file_intents);
            }
            intents.extend(intent);
            if intents.is_empty() {
                match empty_intents_file {
                    Some(path) => bail!(
                        "--intents-file {} contained no intents (every line was blank or a \
                         comment); pass --intent (repeatable) and/or a non-empty --intents-file",
                        path.display()
                    ),
                    None => bail!(
                        "no intent provided: pass --intent (repeatable) and/or --intents-file \
                         <path>"
                    ),
                }
            }

            // a single intent is the pre-existing, unchanged mono-intent path -- built and awaited
            // in-process exactly as before this issue.
            if intents.len() == 1 {
                let intent = intents
                    .into_iter()
                    .next()
                    .expect("checked intents.len() == 1 above");

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

                let tui_launch = tui.then(|| TuiLaunchConfig {
                    tui_bin: resolve_tui_binary(tui_bin),
                });

                let isolation_config = IsolationConfig {
                    isolation,
                    image: isolation_image,
                };

                run(
                    repo,
                    intent,
                    branch,
                    max_review_cycles,
                    max_test_cycles,
                    max_cycles,
                    quota_anticipation_threshold,
                    warden_home,
                    tool,
                    TrustRepoAgents(trust_repo_agents),
                    evidence_tool,
                    evidence_store_in_repo,
                    gate,
                    tui_launch,
                    isolation_config,
                )
                .await
            } else {
                run_batch(BatchCommand {
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
                    tool: tool_as_str(tool),
                    isolation: isolation_as_str(isolation),
                    isolation_image,
                })
                .await
            }
        }
    }
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

    #[test]
    fn parse_approval_answer_accepts_only_y_or_yes_case_insensitively() {
        for approved in ["y", "Y", "yes", "YES", "Yes", "  y  ", "y\n", "y\r\n"] {
            assert!(
                parse_approval_answer(approved),
                "{approved:?} must be accepted"
            );
        }
        for denied in ["", "n", "no", "N", "yess", "ye", " ", "yes please"] {
            assert!(
                !parse_approval_answer(denied),
                "{denied:?} must be denied, fail-closed"
            );
        }
    }

    #[tokio::test]
    async fn no_tui_approval_gate_denies_immediately_without_prompting() {
        let gate = NoTuiApprovalGate;
        let approved = warden::policy_gate::ApprovalGate::approve(
            &gate,
            warden::policy_gate::ApprovalRequest {
                run_id: "run-1",
                description: "git_push to branch \"main\"",
                reason: "push to branch \"main\" requires: tests",
            },
        )
        .await;
        assert!(
            !approved,
            "NoTuiApprovalGate must fail closed while --tui owns the terminal"
        );
    }

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

    #[test]
    fn resolve_tui_binary_prefers_the_explicit_override_when_given() {
        let explicit = PathBuf::from("/some/explicit/path/to/warden-tui");
        assert_eq!(resolve_tui_binary(Some(explicit.clone())), explicit);
    }

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

    #[test]
    fn parse_tool_accepts_claude_codex_and_mistral() {
        assert_eq!(parse_tool("claude"), Ok(ToolName::Claude));
        assert_eq!(parse_tool("codex"), Ok(ToolName::Codex));
        assert_eq!(parse_tool("mistral"), Ok(ToolName::Mistral));
    }

    #[test]
    fn parse_tool_rejects_an_unknown_value_and_lists_every_supported_one() {
        let error = parse_tool("aider").unwrap_err();
        assert!(error.contains("aider"), "{error:?}");
        assert!(error.contains("claude"), "{error:?}");
        assert!(error.contains("codex"), "{error:?}");
        assert!(error.contains("mistral"), "{error:?}");
    }

    #[test]
    fn quota_anticipation_threshold_accepts_fraction_boundaries_only() {
        assert_eq!(parse_quota_anticipation_threshold("0"), Ok(0.0));
        assert_eq!(parse_quota_anticipation_threshold("0.90"), Ok(0.90));
        assert_eq!(parse_quota_anticipation_threshold("1"), Ok(1.0));

        for invalid in ["-0.01", "1.01", "NaN", "inf", "not-a-number"] {
            assert!(
                parse_quota_anticipation_threshold(invalid).is_err(),
                "{invalid:?} must be rejected"
            );
        }
    }
}
