//! Multi-intent batch mode (issue #72): "give N intents, `warden` processes
//! one fully, then kills agents and restarts on a clean context for the
//! next -- zero contamination between tickets".
//!
//! **Isolation strategy (default, chosen for this issue): a fresh `warden run`
//! subprocess per intent.** A brand new OS process gets a brand new
//! `Orchestrator` instance, a brand new `run_id`, and its own
//! `<warden_home>/worktrees/<run_id>/` tree -- there is no in-memory state to
//! carry over between intents by construction, and this crate's own
//! agent-subprocess/worktree teardown (unchanged, exercised by every existing
//! single-intent run) already guarantees agents are killed and worktrees
//! removed once that subprocess's own convergence loop returns. This module
//! only owns the *sequencing* concern layered on top: which intents to run,
//! in what order, what to do when one fails (continue by default, `--fail-fast`
//! to stop), and how to report the outcome -- never a second copy of the
//! agent/worktree lifecycle itself.
//!
//! Everything here is pure (no subprocess spawning, no I/O beyond a single
//! named file read) so it is testable without a real `warden` binary -- the
//! actual spawn loop lives in `main.rs` (the binary), which is the only thing
//! allowed to write to stdout/stderr directly (code-standards.md: "la
//! lib... n'écrit jamais sur stdout/stderr directement").

use std::path::Path;

/// Parses an `--intents-file` (issue #72): one intent per non-blank line.
/// A leading `#` marks a comment line (ignored) -- a convenience for
/// annotating a checked-in intents file, not a format requirement. Blank
/// lines (including whitespace-only ones) are skipped rather than rejected,
/// so trailing newlines or spacing between intents don't need to be exact.
///
/// Never fails: there is no malformed input at this level, only "zero
/// intents found", which the caller (combining this with any `--intent`
/// flags) is responsible for rejecting if the combined total is still zero.
pub fn parse_intents_file(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Everything a single-intent `warden run` child invocation (issue #72's
/// subprocess-per-intent isolation) needs, besides the intent itself --
/// mirrors every other `Commands::Run` flag except `--intent`/
/// `--intents-file`/`--fail-fast`, which are batch-level concerns that never
/// forward as-is (each child gets exactly one `--intent`, no batch flags at
/// all). Plain `&str`/`Option<&str>` rather than `Path`/`PathBuf`: a path
/// that isn't valid UTF-8 can't be forwarded as a CLI argument string either
/// way, so the caller (`main.rs`) converts once, with its own explicit,
/// actionable error if that conversion fails -- the same
/// `to_str().context(...)` shape `attach_warden_home_quoted` already uses,
/// not silently mangled here via `Path::display()`.
pub struct SingleIntentArgs<'a> {
    pub repo: &'a str,
    pub branch: &'a str,
    pub max_review_cycles: u32,
    pub max_test_cycles: u32,
    pub warden_home: &'a str,
    pub tool: &'a str,
    pub trust_repo_agents: bool,
    pub evidence_tool: Option<&'a str>,
    pub evidence_store_in_repo: bool,
    pub gate_bare_repo: Option<&'a str>,
    pub gate_gated_bin: Option<&'a str>,
    pub gate_repo_slug: Option<&'a str>,
    pub gate_poll_interval_secs: u64,
    pub gate_inactivity_timeout_secs: u64,
    pub tui: bool,
    pub tui_bin: Option<&'a str>,
    pub isolation: &'a str,
    pub isolation_image: &'a str,
    /// Number of `-v` occurrences on the parent invocation, forwarded via
    /// repeated `--verbose` so a child's own tracing verbosity matches.
    pub verbose: u8,
}

/// Builds the argv (everything after the child binary's own path) for one
/// `warden run --intent <intent>` child invocation, from `args` plus the one
/// intent this child is responsible for. Pure and deterministic -- no env
/// reads, no filesystem access -- so the exact flags a batch run sends to
/// each child are unit-testable without spawning a real process.
pub fn build_single_intent_args(args: &SingleIntentArgs<'_>, intent: &str) -> Vec<String> {
    let mut out = vec!["run".to_string()];
    for _ in 0..args.verbose {
        out.push("--verbose".to_string());
    }
    out.push("--repo".to_string());
    out.push(args.repo.to_string());
    out.push("--intent".to_string());
    out.push(intent.to_string());
    out.push("--branch".to_string());
    out.push(args.branch.to_string());
    out.push("--max-review-cycles".to_string());
    out.push(args.max_review_cycles.to_string());
    out.push("--max-test-cycles".to_string());
    out.push(args.max_test_cycles.to_string());
    out.push("--warden-home".to_string());
    out.push(args.warden_home.to_string());
    out.push("--tool".to_string());
    out.push(args.tool.to_string());
    if args.trust_repo_agents {
        out.push("--trust-repo-agents".to_string());
    }
    if let Some(evidence_tool) = args.evidence_tool {
        out.push("--evidence-tool".to_string());
        out.push(evidence_tool.to_string());
    }
    out.push("--evidence-store-in-repo".to_string());
    out.push(args.evidence_store_in_repo.to_string());
    if let Some(bare_repo) = args.gate_bare_repo {
        out.push("--gate-bare-repo".to_string());
        out.push(bare_repo.to_string());
    }
    if let Some(gated_bin) = args.gate_gated_bin {
        out.push("--gate-gated-bin".to_string());
        out.push(gated_bin.to_string());
    }
    if let Some(repo_slug) = args.gate_repo_slug {
        out.push("--gate-repo-slug".to_string());
        out.push(repo_slug.to_string());
    }
    out.push("--gate-poll-interval-secs".to_string());
    out.push(args.gate_poll_interval_secs.to_string());
    out.push("--gate-inactivity-timeout-secs".to_string());
    out.push(args.gate_inactivity_timeout_secs.to_string());
    if args.tui {
        out.push("--tui".to_string());
    }
    if let Some(tui_bin) = args.tui_bin {
        out.push("--tui-bin".to_string());
        out.push(tui_bin.to_string());
    }
    out.push("--isolation".to_string());
    out.push(args.isolation.to_string());
    out.push("--isolation-image".to_string());
    out.push(args.isolation_image.to_string());
    out
}

/// Parses a `warden run` child's `"run <id> started"` stdout line (see
/// `main.rs::print_run_started_hint`), returning the run id. Lets the batch
/// runner record which run a since-crashed/killed child was even attempting
/// -- e.g. reporting a subprocess crash before it produced its own `"...
/// finished: ..."` line -- without re-deriving the parsing convention `tests/
/// cli.rs::extract_run_id` already established for it.
pub fn parse_started_line(line: &str) -> Option<&str> {
    line.strip_prefix("run ")?.strip_suffix(" started")
}

/// Parses a `warden run` child's final `"run <id> finished: <State>"` stdout
/// line (see `main.rs::run`'s own final `print_stdout_line_or_log` call),
/// returning `(run_id, final_state)`. `final_state` is `RunState`'s `Debug`
/// form (e.g. `"Converged"`, `"MaxReviewCyclesExceeded"`), never re-parsed
/// into an actual `RunState` here -- this module only needs to tell success
/// apart from every other outcome ([`is_converged_state`]), not the full
/// state machine.
pub fn parse_finished_line(line: &str) -> Option<(&str, &str)> {
    line.strip_prefix("run ")?.split_once(" finished: ")
}

/// Whether `final_state` (a `RunState` `Debug` string, see
/// [`parse_finished_line`]) counts as this intent having actually converged.
/// `Done` (the post-gate terminal state, ADR-0011) counts alongside
/// `Converged` (the no-gate terminal state) -- both mean the run's own goal
/// was reached, just with or without the post-`Converged` push/PR/CI tail
/// configured. Every other value (`MaxReviewCyclesExceeded`,
/// `MaxTestCyclesExceeded`, `Failed`, or anything else) is not a success.
pub fn is_converged_state(final_state: &str) -> bool {
    matches!(final_state, "Converged" | "Done")
}

/// Outcome of one intent's isolated child run, once known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentStatus {
    /// The child exited successfully and its run reached `Converged`/`Done`.
    Converged { final_state: String },
    /// The child exited successfully, but its run ended in a non-converged
    /// terminal state (`MaxReviewCyclesExceeded`/`MaxTestCyclesExceeded`/
    /// `Failed`/other) -- not a crash, just "didn't converge".
    NotConverged { final_state: String },
    /// The child either exited non-zero, or exited zero but never printed a
    /// parseable `"... finished: ..."` line at all (a bug, or a crash after
    /// that print but before flush -- either way, this batch run cannot
    /// trust that intent's outcome).
    SubprocessError { reason: String },
    /// Never attempted: batch stopped at an earlier failing intent under
    /// `--fail-fast`.
    Skipped,
}

impl IntentStatus {
    /// Whether this intent counts as a success for the batch's own final
    /// exit code and `X/N converged` tally.
    pub fn is_success(&self) -> bool {
        matches!(self, IntentStatus::Converged { .. })
    }
}

/// One intent's outcome within a batch run (issue #72's "per-intent status"
/// acceptance criterion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentReport {
    pub intent: String,
    /// `None` only for [`IntentStatus::Skipped`], or a
    /// [`IntentStatus::SubprocessError`] so early the child never even
    /// printed its own `"... started"` line.
    pub run_id: Option<String>,
    pub status: IntentStatus,
}

/// Renders the final batch summary (issue #72's "final report listing each
/// intent's result") as plain text -- the actual `println!` happens in
/// `main.rs`, this only builds the string so its exact shape is unit
/// testable.
pub fn summarize(reports: &[IntentReport]) -> String {
    let converged = reports.iter().filter(|r| r.status.is_success()).count();
    let mut lines = vec![format!(
        "batch summary: {converged}/{} intent(s) converged",
        reports.len()
    )];
    for (index, report) in reports.iter().enumerate() {
        let run_id = report.run_id.as_deref().unwrap_or("-");
        let outcome = match &report.status {
            IntentStatus::Converged { final_state } => format!("{final_state} (run {run_id})"),
            IntentStatus::NotConverged { final_state } => {
                format!("FAILED -- {final_state} (run {run_id})")
            }
            IntentStatus::SubprocessError { reason } => {
                format!("FAILED -- {reason} (run {run_id})")
            }
            IntentStatus::Skipped => {
                "SKIPPED -- earlier intent failed under --fail-fast".to_string()
            }
        };
        lines.push(format!(
            "  [{}/{}] {:?}: {outcome}",
            index + 1,
            reports.len(),
            report.intent
        ));
    }
    lines.join("\n")
}

/// Whether the whole batch should be reported as failed (issue #72: the
/// batch's own final exit code) -- any intent that didn't converge, whether
/// skipped, crashed, or simply exhausted its budget.
pub fn batch_failed(reports: &[IntentReport]) -> bool {
    reports.iter().any(|report| !report.status.is_success())
}

/// Reads and parses `path` as an `--intents-file` (issue #72). Kept as a thin
/// wrapper around [`parse_intents_file`] so the one fallible part (the
/// filesystem read) has a single, named call site -- `main.rs` maps its
/// `io::Error` into its own `anyhow::Context`, naming `path`.
pub fn read_intents_file(path: &Path) -> std::io::Result<Vec<String>> {
    let contents = std::fs::read_to_string(path)?;
    Ok(parse_intents_file(&contents))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_intents_file_skips_blank_lines_and_comments() {
        let contents = "\n  \nfirst intent\n# a comment\n   second intent   \n#also a comment\n";
        assert_eq!(
            parse_intents_file(contents),
            vec!["first intent".to_string(), "second intent".to_string()]
        );
    }

    #[test]
    fn parse_intents_file_returns_empty_for_an_all_comment_file() {
        assert_eq!(
            parse_intents_file("# nothing here\n\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn build_single_intent_args_includes_the_intent_and_every_forwarded_flag() {
        let args = SingleIntentArgs {
            repo: "/repo",
            branch: "main",
            max_review_cycles: 5,
            max_test_cycles: 5,
            warden_home: "/home/.warden",
            tool: "claude",
            trust_repo_agents: true,
            evidence_tool: Some("playwright"),
            evidence_store_in_repo: true,
            gate_bare_repo: Some("/bare.git"),
            gate_gated_bin: Some("/bin/warden-gated"),
            gate_repo_slug: Some("acme/widgets"),
            gate_poll_interval_secs: 15,
            gate_inactivity_timeout_secs: 1800,
            tui: false,
            tui_bin: None,
            isolation: "worktree",
            isolation_image: "warden-agent:latest",
            verbose: 2,
        };

        let built = build_single_intent_args(&args, "fix the thing");

        assert_eq!(
            built,
            vec![
                "run",
                "--verbose",
                "--verbose",
                "--repo",
                "/repo",
                "--intent",
                "fix the thing",
                "--branch",
                "main",
                "--max-review-cycles",
                "5",
                "--max-test-cycles",
                "5",
                "--warden-home",
                "/home/.warden",
                "--tool",
                "claude",
                "--trust-repo-agents",
                "--evidence-tool",
                "playwright",
                "--evidence-store-in-repo",
                "true",
                "--gate-bare-repo",
                "/bare.git",
                "--gate-gated-bin",
                "/bin/warden-gated",
                "--gate-repo-slug",
                "acme/widgets",
                "--gate-poll-interval-secs",
                "15",
                "--gate-inactivity-timeout-secs",
                "1800",
                "--isolation",
                "worktree",
                "--isolation-image",
                "warden-agent:latest",
            ]
        );
    }

    #[test]
    fn build_single_intent_args_omits_absent_optionals() {
        let args = SingleIntentArgs {
            repo: "/repo",
            branch: "main",
            max_review_cycles: 5,
            max_test_cycles: 5,
            warden_home: "/home/.warden",
            tool: "claude",
            trust_repo_agents: false,
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate_bare_repo: None,
            gate_gated_bin: None,
            gate_repo_slug: None,
            gate_poll_interval_secs: 15,
            gate_inactivity_timeout_secs: 1800,
            tui: true,
            tui_bin: Some("/bin/warden-tui"),
            isolation: "docker",
            isolation_image: "warden-agent:latest",
            verbose: 0,
        };

        let built = build_single_intent_args(&args, "do it");

        assert!(!built.contains(&"--trust-repo-agents".to_string()));
        assert!(!built.contains(&"--evidence-tool".to_string()));
        assert!(!built.contains(&"--gate-bare-repo".to_string()));
        assert!(!built.contains(&"--gate-gated-bin".to_string()));
        assert!(!built.contains(&"--gate-repo-slug".to_string()));
        assert!(!built.contains(&"--verbose".to_string()));
        assert!(built.contains(&"--tui".to_string()));
        assert!(built.contains(&"--tui-bin".to_string()));
        assert!(built.contains(&"/bin/warden-tui".to_string()));
    }

    #[test]
    fn parse_started_line_extracts_the_run_id() {
        assert_eq!(parse_started_line("run abc-123 started"), Some("abc-123"));
        assert_eq!(parse_started_line("not a started line"), None);
    }

    #[test]
    fn parse_finished_line_extracts_run_id_and_final_state() {
        assert_eq!(
            parse_finished_line("run abc-123 finished: Converged"),
            Some(("abc-123", "Converged"))
        );
        assert_eq!(
            parse_finished_line("run abc-123 finished: MaxReviewCyclesExceeded"),
            Some(("abc-123", "MaxReviewCyclesExceeded"))
        );
        assert_eq!(parse_finished_line("attach: warden-tui attach ..."), None);
    }

    #[test]
    fn is_converged_state_accepts_converged_and_done_only() {
        assert!(is_converged_state("Converged"));
        assert!(is_converged_state("Done"));
        assert!(!is_converged_state("MaxReviewCyclesExceeded"));
        assert!(!is_converged_state("MaxTestCyclesExceeded"));
        assert!(!is_converged_state("Failed"));
    }

    #[test]
    fn batch_failed_is_false_only_when_every_intent_converged() {
        let all_converged = vec![
            IntentReport {
                intent: "a".to_string(),
                run_id: Some("1".to_string()),
                status: IntentStatus::Converged {
                    final_state: "Converged".to_string(),
                },
            },
            IntentReport {
                intent: "b".to_string(),
                run_id: Some("2".to_string()),
                status: IntentStatus::Converged {
                    final_state: "Done".to_string(),
                },
            },
        ];
        assert!(!batch_failed(&all_converged));

        let one_failed = vec![
            all_converged[0].clone(),
            IntentReport {
                intent: "c".to_string(),
                run_id: Some("3".to_string()),
                status: IntentStatus::NotConverged {
                    final_state: "MaxReviewCyclesExceeded".to_string(),
                },
            },
        ];
        assert!(batch_failed(&one_failed));
    }

    #[test]
    fn summarize_lists_every_intent_with_its_outcome_and_a_tally() {
        let reports = vec![
            IntentReport {
                intent: "first".to_string(),
                run_id: Some("run-1".to_string()),
                status: IntentStatus::Converged {
                    final_state: "Converged".to_string(),
                },
            },
            IntentReport {
                intent: "second".to_string(),
                run_id: Some("run-2".to_string()),
                status: IntentStatus::NotConverged {
                    final_state: "MaxReviewCyclesExceeded".to_string(),
                },
            },
            IntentReport {
                intent: "third".to_string(),
                run_id: None,
                status: IntentStatus::Skipped,
            },
        ];

        let summary = summarize(&reports);
        assert!(summary.starts_with("batch summary: 1/3 intent(s) converged"));
        assert!(summary.contains("[1/3] \"first\": Converged (run run-1)"));
        assert!(summary.contains("[2/3] \"second\": FAILED -- MaxReviewCyclesExceeded (run run-2)"));
        assert!(
            summary.contains("[3/3] \"third\": SKIPPED -- earlier intent failed under --fail-fast")
        );
    }

    #[test]
    fn read_intents_file_surfaces_a_missing_file_as_a_typed_io_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = read_intents_file(&dir.path().join("does-not-exist.txt"));
        assert!(result.is_err());
    }

    #[test]
    fn read_intents_file_reads_and_parses_a_real_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("intents.txt");
        std::fs::write(&path, "one\n# skip\ntwo\n").unwrap();
        assert_eq!(
            read_intents_file(&path).unwrap(),
            vec!["one".to_string(), "two".to_string()]
        );
    }
}
