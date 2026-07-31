//! `warden` binary: CLI parsing + dispatch only. All orchestration logic
//! lives in the `warden` library crate (`src/lib.rs` and friends).

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
            // Issue #72: `--intents-file` entries run first (file order),
            // followed by any repeated `--intent` flags, in the order given.
            let mut intents = Vec::new();
            // Issue #72 review, LOW 2: tracked separately from `intents`
            // itself so the final "no intent provided" error (below) can
            // name the file specifically when it's the reason the combined
            // list is empty -- e.g. an all-comment/blank `--intents-file`
            // with no `--intent` flags at all reads very differently from
            // "you forgot both flags entirely".
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

            // Issue #72: a single intent is the pre-existing, unchanged
            // mono-intent path -- built and awaited in-process exactly as
            // before this issue. Two or more intents switch to `run_batch`,
            // which never builds a `RunConfig`/`Orchestrator` itself; each
            // intent gets its own `warden run` subprocess instead (see
            // `run_batch`'s own docs for why). The intent-count branch wraps
            // the `--tool` dispatch (issue #71) so every adapter -- and the
            // batch runner -- shares this same single-vs-batch decision.
            if intents.len() == 1 {
                let intent = intents
                    .into_iter()
                    .next()
                    .expect("checked intents.len() == 1 above");

                // Issue #15/ADR-0011: the post-Converged tail only runs when
                // both paths it needs are configured; omitting either
                // preserves this crate's original behaviour (stop at
                // `Converged`).
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

    /// Issue #51: only an explicit affirmative answer approves --
    /// `TtyApprovalGate`'s human-validation wait point must fail closed on
    /// anything else, including a bare newline (the operator just pressed
    /// enter) or a typo.
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

    /// Issue #51 review round 2, finding A: `NoTuiApprovalGate` -- installed
    /// whenever `--tui` is attached -- must deny a `RequireApproval` decision
    /// immediately, with no attempt to read stdin/prompt a terminal. This is
    /// the one behaviour `TtyApprovalGate`'s own docs say a real prompt would
    /// get wrong under `--tui` (raw mode swallows `\n`, so a `read_line`
    /// there would simply hang forever). A test that resolves at all --
    /// rather than timing out -- is itself the proof there is no hidden
    /// blocking read on this path; the explicit `false` assertion additionally
    /// pins the fail-closed contract.
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

    /// Issue #85: the quota suspension threshold is user input, so its
    /// boundary values are valid while non-finite and out-of-range values
    /// fail at the CLI boundary rather than reaching the orchestrator.
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
