//! `warden-gated` binary: CLI parsing + dispatch only. All gate logic
//! (read-only re-verification, push, hook installation) lives in the
//! library crate (`src/lib.rs` and friends).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use warden_core::CiResultMessage;
use warden_gated::ci_watcher::{watch_pr, WatchConfig, WatchOutcome};
use warden_gated::gate::verify_and_authorize;
use warden_gated::gh_provider::GhProvider;
use warden_gated::pr_manager::PrHandle;
use warden_gated::run_tail::RunTailRequest;
use warden_gated::verify::GateDecision;
use warden_gated::{bare_repo, ci_report, db, hook, relay, run_tail, serve};

#[derive(Parser)]
#[command(
    name = "warden-gated",
    version,
    about = "Git gate daemon: sole holder of origin credentials (ADR-0002/ADR-0006)"
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
    /// Creates the local bare gate repo and installs its `post-receive`
    /// hook (ADR-0002). Safe to re-run: creating the repo is a no-op if it
    /// already exists, and the hook file is always rewritten.
    InitBare {
        /// Path to the local bare gate repo (created if missing).
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        bare_repo: PathBuf,

        /// Absolute path to the installed `warden-gated` binary -- baked
        /// into the generated hook script.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        bin: PathBuf,

        /// Unix socket the `serve` daemon listens on -- baked into the
        /// generated hook script.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        socket: PathBuf,

        /// URL/path of the real remote to configure as `origin` inside the
        /// bare gate repo. Its credentials are never handled by this
        /// command -- whatever the machine's git/SSH config already
        /// provides at push time.
        #[arg(long)]
        origin_url: Option<String>,
    },

    /// Runs the long-running daemon: accepts relayed `post-receive`
    /// payloads and independently re-verifies each run against SQLite
    /// (read-only) before ever pushing to `origin`. Intended to run as a
    /// managed service -- see `contrib/systemd` and `contrib/launchd`.
    Serve {
        /// Unix socket to listen on for relayed hook notifications.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        socket: PathBuf,

        /// `warden`'s SQLite database, opened read-only.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        db: PathBuf,

        /// The local bare gate repo to push from.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        bare_repo: PathBuf,

        /// Branch to update on `origin` once a push is authorized.
        #[arg(long, default_value = "main")]
        branch: String,
    },

    /// Minimal relay invoked by the installed `post-receive` hook: forwards
    /// stdin verbatim to the `serve` daemon's socket. Contains no parsing
    /// or decision logic of its own (ADR-0002: "aucune logique métier dans
    /// le hook").
    Notify {
        /// Unix socket the `serve` daemon is listening on.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        socket: PathBuf,
    },

    /// Diagnostic: independently re-verifies a single run against SQLite
    /// (read-only) and prints the gate's decision, without touching
    /// `origin`. Exit code is 0 for `Allow`, 1 for `Blocked`.
    VerifyRun {
        /// `warden`'s SQLite database, opened read-only.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        db: PathBuf,

        /// The run id to re-verify.
        #[arg(long)]
        run_id: String,

        /// The commit sha to check against `runs.converged_commit_sha`
        /// (stands in for the commit actually pushed into the bare repo).
        #[arg(long)]
        commit: String,
    },

    /// CI Watcher (issue #5): polls an already-opened PR until a terminal
    /// status is reached -- merged, closed, checks-passed, checks-failed, or
    /// an inactivity timeout -- and reports that outcome. Never merges the
    /// PR itself: once checks pass, the merge decision is left entirely to a
    /// human via the PR provider's own UI.
    WatchPr {
        /// The local bare gate repo, used to resolve `owner/repo` from its
        /// `origin` remote when `--repo` isn't given (same resolution
        /// `GhProvider::new` already does for `OpenDraft`/`Finalize`).
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        bare_repo: PathBuf,

        /// Explicit `owner/repo` override, bypassing `origin` remote
        /// detection.
        #[arg(long)]
        repo: Option<String>,

        /// The PR number to watch.
        #[arg(long)]
        pr: u64,

        /// Seconds to sleep between two polls. Never busy-spins.
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..))]
        poll_interval_secs: u64,

        /// Seconds the polled status may go completely unchanged before
        /// giving up (inactivity timeout) -- issue #5: "pas d'attente
        /// infinie".
        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..))]
        inactivity_timeout_secs: u64,
    },

    /// Issue #15/ADR-0011: runs a converged run's post-push tail (skeleton
    /// commit + `OpenDraft` + `Finalize` + `watch_pr`) and delivers the one
    /// terminal `CiResultMessage` to `warden`'s reverse socket. `warden`
    /// spawns this as a subprocess once it has pushed the converged commit
    /// into the bare gate repo -- itself never touches `origin`/PR
    /// credentials (ADR-0006); this command independently re-verifies the
    /// run via `Finalize`'s own `verify_and_authorize` before pushing
    /// anything. The PR body's summary is read from stdin.
    RunTail {
        #[arg(long)]
        run_id: String,

        /// `warden`'s SQLite database, opened read-only.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        db: PathBuf,

        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        bare_repo: PathBuf,

        /// The run's own branch, pushed under both the skeleton and the
        /// final content (e.g. `warden/<run_id>`).
        #[arg(long)]
        branch: String,

        /// The branch this run's PR targets (e.g. `main`).
        #[arg(long)]
        base_branch: String,

        #[arg(long)]
        intent: String,

        /// The commit already pushed into the bare gate repo.
        #[arg(long)]
        pushed_commit: String,

        /// Explicit `owner/repo` override, bypassing `origin` remote
        /// detection.
        #[arg(long)]
        repo: Option<String>,

        /// `warden`'s reverse-channel socket to deliver the terminal
        /// `CiResultMessage` to.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        ci_result_socket: PathBuf,

        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..))]
        poll_interval_secs: u64,

        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..))]
        inactivity_timeout_secs: u64,

        /// Evidence rows captured for this run (issue #15 review, M2),
        /// JSON-encoded (`warden_core::serialize_evidence_rows`) -- folded
        /// into the finalized PR body's Evidence section (ADR-0009) when
        /// non-empty. Defaults to no evidence.
        #[arg(long, default_value = "[]")]
        evidence_json: String,

        /// The PR already opened for this run in an earlier attempt (issue
        /// #15 review, H3): when present, this run skips the skeleton
        /// commit and `OpenDraft` entirely and pushes straight to
        /// `Finalize` against the existing PR -- opening a second draft PR
        /// for the same branch would be rejected by a real PR provider.
        #[arg(long)]
        existing_pr_number: Option<u64>,
    },

    /// Issue #15/ADR-0011 crash-recovery counterpart of `run-tail`: the PR
    /// was already opened/finalized in an earlier attempt (its number is
    /// read back from `warden`'s own `runs.pr_number`), so this only resumes
    /// `watch_pr` and delivers the resulting `CiResultMessage` --
    /// `warden-gated` keeps no watch state of its own between attempts.
    /// Independently re-verifies the run is still `AwaitingCi` before doing
    /// anything (never trusts the caller).
    ResumeWatch {
        #[arg(long)]
        run_id: String,

        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        db: PathBuf,

        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        bare_repo: PathBuf,

        #[arg(long)]
        repo: Option<String>,

        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        ci_result_socket: PathBuf,

        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..))]
        poll_interval_secs: u64,

        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..))]
        inactivity_timeout_secs: u64,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Commands::InitBare {
            bare_repo,
            bin,
            socket,
            origin_url,
        } => init_bare(bare_repo, bin, socket, origin_url).await,
        Commands::Serve {
            socket,
            db,
            bare_repo,
            branch,
        } => run_serve(socket, db, bare_repo, branch).await,
        Commands::Notify { socket } => notify(socket).await,
        Commands::VerifyRun { db, run_id, commit } => verify_run(db, run_id, commit).await,
        Commands::WatchPr {
            bare_repo,
            repo,
            pr,
            poll_interval_secs,
            inactivity_timeout_secs,
        } => {
            watch_pr_cmd(
                bare_repo,
                repo,
                pr,
                poll_interval_secs,
                inactivity_timeout_secs,
            )
            .await
        }
        Commands::RunTail {
            run_id,
            db,
            bare_repo,
            branch,
            base_branch,
            intent,
            pushed_commit,
            repo,
            ci_result_socket,
            poll_interval_secs,
            inactivity_timeout_secs,
            evidence_json,
            existing_pr_number,
        } => {
            run_tail_cmd(
                run_id,
                db,
                bare_repo,
                branch,
                base_branch,
                intent,
                pushed_commit,
                repo,
                ci_result_socket,
                poll_interval_secs,
                inactivity_timeout_secs,
                evidence_json,
                existing_pr_number,
            )
            .await
        }
        Commands::ResumeWatch {
            run_id,
            db,
            bare_repo,
            repo,
            ci_result_socket,
            poll_interval_secs,
            inactivity_timeout_secs,
        } => {
            resume_watch_cmd(
                run_id,
                db,
                bare_repo,
                repo,
                ci_result_socket,
                poll_interval_secs,
                inactivity_timeout_secs,
            )
            .await
        }
    }
}

async fn init_bare(
    bare_repo: PathBuf,
    bin: PathBuf,
    socket: PathBuf,
    origin_url: Option<String>,
) -> anyhow::Result<()> {
    bare_repo::init(&bare_repo, origin_url.as_deref())
        .await
        .context("failed to initialize the bare gate repo")?;
    hook::install(&bare_repo, &bin, &socket)
        .await
        .context("failed to install the post-receive hook")?;
    println!(
        "bare gate repo ready at {} (post-receive hook installed)",
        bare_repo.display()
    );
    Ok(())
}

async fn run_serve(
    socket: PathBuf,
    db: PathBuf,
    bare_repo: PathBuf,
    branch: String,
) -> anyhow::Result<()> {
    serve::serve(serve::ServeConfig {
        socket_path: socket,
        db_path: db,
        bare_repo_path: bare_repo,
        target_branch: branch,
    })
    .await
    .context("warden-gated daemon exited with an error")
}

/// Reads all of stdin (the `post-receive` hook's payload) and relays it
/// verbatim to the daemon's socket -- no interpretation here, see
/// `Commands::Notify`'s doc comment.
async fn notify(socket: PathBuf) -> anyhow::Result<()> {
    let mut payload = Vec::new();
    std::io::stdin()
        .read_to_end(&mut payload)
        .context("failed to read post-receive payload from stdin")?;

    relay::relay(&socket, &payload)
        .await
        .context("failed to relay push notification to warden-gated")?;
    Ok(())
}

async fn verify_run(db: PathBuf, run_id: String, commit: String) -> anyhow::Result<()> {
    let pool = db::connect_read_only(&db)
        .await
        .context("failed to open warden's database read-only")?;

    let decision = verify_and_authorize(&pool, &run_id, &commit)
        .await
        .context("failed to re-verify the run")?;

    match decision {
        GateDecision::Allow { commit_sha } => {
            println!("ALLOW: run {run_id} converged on {commit_sha}");
            Ok(())
        }
        GateDecision::Blocked(reason) => {
            println!("BLOCKED: {reason:?}");
            bail!("push blocked for run {run_id}: {reason:?}");
        }
    }
}

/// Watches `pr` until a terminal outcome, then prints it and maps it to an
/// exit code: `0` for `Merged`/`ChecksPassed` (the two outcomes that need no
/// further action from Warden itself), non-zero otherwise -- mirroring
/// `verify_run`'s `Allow`/`Blocked` exit-code convention. Never merges the
/// PR (issue #5): `ChecksPassed` is reported and left to a human, exactly
/// like `Merged` is.
async fn watch_pr_cmd(
    bare_repo: PathBuf,
    repo: Option<String>,
    pr_number: u64,
    poll_interval_secs: u64,
    inactivity_timeout_secs: u64,
) -> anyhow::Result<()> {
    let provider = GhProvider::new(&bare_repo, repo.as_deref())
        .await
        .context("failed to resolve the GitHub repo to watch")?;
    let pr = PrHandle { number: pr_number };
    let config = WatchConfig {
        poll_interval: Duration::from_secs(poll_interval_secs),
        inactivity_timeout: Duration::from_secs(inactivity_timeout_secs),
        max_consecutive_poll_errors: WatchConfig::DEFAULT_MAX_CONSECUTIVE_POLL_ERRORS,
    };

    let outcome = watch_pr(&pr, &provider, &config)
        .await
        .context("failed while watching the pr")?;

    match outcome {
        WatchOutcome::Merged => {
            println!("MERGED: pr #{pr_number}");
            Ok(())
        }
        WatchOutcome::ChecksPassed => {
            println!("CHECKS-PASSED: pr #{pr_number} (merge decision left to a human)");
            Ok(())
        }
        WatchOutcome::Closed => {
            println!("CLOSED: pr #{pr_number} (closed without merging)");
            bail!("pr #{pr_number} was closed without merging");
        }
        WatchOutcome::ChecksFailed(findings) => {
            println!("CHECKS-FAILED: pr #{pr_number}");
            for finding in &findings {
                println!("- [{}] {}", finding.severity.as_str(), finding.description);
            }
            bail!("pr #{pr_number} has failing CI checks");
        }
        WatchOutcome::TimedOut => {
            println!(
                "TIMED-OUT: pr #{pr_number} (no status change for {inactivity_timeout_secs}s)"
            );
            bail!("timed out watching pr #{pr_number}: no status change for {inactivity_timeout_secs}s");
        }
    }
}

/// Issue #15/ADR-0011 review (H1): runs `attempt`, guaranteeing a terminal
/// [`CiResultMessage`] is delivered to `ci_result_socket` regardless of
/// whether `attempt` returns `Ok` or `Err`. `warden` blocks in
/// `CiResultListener::receive()` waiting for exactly one message per run --
/// without this, an early setup failure (stdin, `db::connect_read_only`,
/// `GhProvider::new`, ...) that returns a bare `Err` *before* ever reaching
/// `run_tail`/`resume_watch`'s own internal `GateFailed` handling would
/// leave `warden` hanging forever, since those two functions already never
/// propagate a bare error themselves (see `run_tail::run_tail`'s docs) --
/// this closes the one gap upstream of them.
async fn deliver_result_or_gate_failed<F>(
    run_id: &str,
    ci_result_socket: &Path,
    attempt: F,
) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<CiResultMessage>>,
{
    let message = match attempt.await {
        Ok(message) => message,
        Err(error) => {
            tracing::error!(
                run_id,
                %error,
                "tail setup failed before producing a terminal outcome; reporting GateFailed"
            );
            CiResultMessage {
                run_id: run_id.to_string(),
                pr_number: None,
                outcome: warden_core::CiWatchOutcome::gate_failed(error.to_string()),
            }
        }
    };

    let is_gate_failed = matches!(
        message.outcome,
        warden_core::CiWatchOutcome::GateFailed { .. }
    );
    ci_report::send_ci_result(ci_result_socket, &message)
        .await
        .context("failed to deliver the CI result to warden")?;

    if is_gate_failed {
        bail!(
            "tail for run {run_id} ended in GateFailed -- see the delivered CiResultMessage for \
             the reason"
        );
    }
    Ok(())
}

/// Issue #15/ADR-0011: runs the fresh post-Converged tail and delivers the
/// resulting `CiResultMessage`. The PR body's summary is read from stdin
/// (mirrors `Commands::Notify`'s "read stdin, no CLI-arg length/escaping
/// concerns" convention). Evidence rows (issue #15 review, M2) are passed as
/// a JSON-encoded `--evidence-json` argument -- structured, bounded data,
/// unlike the free-text summary, so a CLI argument (rather than sharing
/// stdin with the summary via some ad hoc envelope) is the simpler choice.
#[allow(clippy::too_many_arguments)]
async fn run_tail_cmd(
    run_id: String,
    db: PathBuf,
    bare_repo: PathBuf,
    branch: String,
    base_branch: String,
    intent: String,
    pushed_commit: String,
    repo: Option<String>,
    ci_result_socket: PathBuf,
    poll_interval_secs: u64,
    inactivity_timeout_secs: u64,
    evidence_json: String,
    existing_pr_number: Option<u64>,
) -> anyhow::Result<()> {
    deliver_result_or_gate_failed(&run_id, &ci_result_socket, async {
        let mut summary_body = String::new();
        std::io::stdin()
            .read_to_string(&mut summary_body)
            .context("failed to read the PR summary body from stdin")?;

        let evidence = warden_core::parse_evidence_rows(&evidence_json)
            .context("failed to parse --evidence-json")?;

        let pool = db::connect_read_only(&db)
            .await
            .context("failed to open warden's database read-only")?;
        let provider = GhProvider::new(&bare_repo, repo.as_deref())
            .await
            .context("failed to resolve the GitHub repo for this run's PR")?;

        let request = RunTailRequest {
            bare_repo_path: &bare_repo,
            run_id: &run_id,
            intent: &intent,
            branch: &branch,
            base_branch: &base_branch,
            pushed_commit_sha: &pushed_commit,
            summary_body: &summary_body,
            evidence: &evidence,
            repo_slug: provider.repo_slug(),
            existing_pr_number,
            watch_config: WatchConfig {
                poll_interval: Duration::from_secs(poll_interval_secs),
                inactivity_timeout: Duration::from_secs(inactivity_timeout_secs),
                max_consecutive_poll_errors: WatchConfig::DEFAULT_MAX_CONSECUTIVE_POLL_ERRORS,
            },
        };

        Ok(run_tail::run_tail(&pool, &request, &provider).await)
    })
    .await
}

/// Issue #15/ADR-0011: crash-recovery counterpart of `run_tail_cmd`.
/// Independently re-reads `run_id`'s state before resuming anything -- never
/// trusts that the caller's belief ("this run is stuck in AwaitingCi") is
/// still true (code-standards.md: "warden-gated ... revérifie systématiquement
/// l'état du run"). Also guarantees a terminal message is delivered even
/// when that re-verification itself refuses to resume (issue #15 review,
/// H1) -- see `deliver_result_or_gate_failed`.
async fn resume_watch_cmd(
    run_id: String,
    db: PathBuf,
    bare_repo: PathBuf,
    repo: Option<String>,
    ci_result_socket: PathBuf,
    poll_interval_secs: u64,
    inactivity_timeout_secs: u64,
) -> anyhow::Result<()> {
    deliver_result_or_gate_failed(&run_id, &ci_result_socket, async {
        let pool = db::connect_read_only(&db)
            .await
            .context("failed to open warden's database read-only")?;
        let run = db::get_awaiting_ci_run_view(&pool, &run_id)
            .await
            .context("failed to re-read the run's state")?
            .with_context(|| format!("run {run_id} not found"))?;

        let pr_number = match (run.state, run.pr_number) {
            (warden_core::RunState::AwaitingCi, Some(pr_number)) => pr_number,
            (state, pr_number) => {
                bail!(
                    "refusing to resume watching run {run_id}: state is {state:?} (expected \
                     AwaitingCi) and pr_number is {pr_number:?} (expected Some) -- this run is \
                     no longer a valid resume-watch target"
                );
            }
        };

        let provider = GhProvider::new(&bare_repo, repo.as_deref())
            .await
            .context("failed to resolve the GitHub repo for this run's PR")?;
        let config = WatchConfig {
            poll_interval: Duration::from_secs(poll_interval_secs),
            inactivity_timeout: Duration::from_secs(inactivity_timeout_secs),
            max_consecutive_poll_errors: WatchConfig::DEFAULT_MAX_CONSECUTIVE_POLL_ERRORS,
        };

        Ok(run_tail::resume_watch(&run_id, pr_number, &provider, &config).await)
    })
    .await
}

fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("warden_gated={level}")));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    /// Issue #15 review, H1: an early setup failure -- `stdin`,
    /// `db::connect_read_only`, `GhProvider::new`, `--evidence-json`
    /// parsing, ... -- that returns a bare `Err` *before* `run_tail`/
    /// `resume_watch` ever get a chance to run their own internal
    /// `GateFailed` handling must still result in exactly one terminal
    /// `CiResultMessage` delivered to `warden`'s reverse socket, carrying a
    /// `GateFailed` outcome -- never silence, which would leave `warden`
    /// blocked forever in `CiResultListener::receive()`. This exercises
    /// `deliver_result_or_gate_failed` directly (the wrapper both
    /// `run_tail_cmd` and `resume_watch_cmd` are built on) against a real
    /// Unix socket, standing in for `warden`'s own listener.
    #[tokio::test]
    async fn deliver_result_or_gate_failed_still_delivers_a_terminal_message_on_early_setup_failure(
    ) {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("warden.ci.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        // Bounded rather than an unbounded `accept().await`: if this
        // regressed back to the bare-`Err`-with-no-delivery behavior this
        // wrapper exists to close, nothing would ever connect, and an
        // unbounded wait here would just hang this test (and, in
        // production, `warden`'s own `CiResultListener::receive`) instead
        // of failing it cleanly.
        let receiver = async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let (mut stream, _addr) = listener.accept().await.unwrap();
                let mut buffer = String::new();
                stream.read_to_string(&mut buffer).await.unwrap();
                buffer
            })
            .await
        };

        let attempt = async {
            // Stands in for a setup step that fails before ever reaching
            // run_tail/resume_watch's own internal GateFailed handling
            // (e.g. GhProvider::new against an unreachable repo, or a
            // malformed --evidence-json), returning a bare `Err` -- exactly
            // the gap this wrapper closes.
            Err(anyhow::anyhow!("simulated early setup failure"))
        };

        let (received_json, attempt_result) = tokio::join!(
            receiver,
            deliver_result_or_gate_failed("run-1", &socket_path, attempt)
        );

        assert!(
            attempt_result.is_err(),
            "a GateFailed outcome must still surface as an error to the caller (non-zero exit)"
        );

        let received_json = received_json.expect(
            "no terminal message was ever delivered within the timeout -- this is exactly the \
             hang this wrapper exists to prevent",
        );
        let message: CiResultMessage = warden_core::parse_ci_result_message(&received_json)
            .expect("a terminal message must have been delivered and must parse");
        assert_eq!(message.run_id, "run-1");
        assert!(
            matches!(
                message.outcome,
                warden_core::CiWatchOutcome::GateFailed { .. }
            ),
            "an early setup failure must be reported as GateFailed, not silently dropped: \
             {message:?}"
        );
    }

    /// The success path: when `attempt` itself already produced a terminal
    /// message (mirrors `run_tail`/`resume_watch`'s own contract of never
    /// returning a bare `Err`), that exact message is delivered unmodified
    /// and `deliver_result_or_gate_failed` returns `Ok` for any non-
    /// `GateFailed` outcome.
    #[tokio::test]
    async fn deliver_result_or_gate_failed_delivers_the_attempts_own_message_unmodified() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("warden.ci.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let receiver = async {
            let (mut stream, _addr) = listener.accept().await.unwrap();
            let mut buffer = String::new();
            stream.read_to_string(&mut buffer).await.unwrap();
            buffer
        };

        let attempt = async {
            Ok(CiResultMessage {
                run_id: "run-1".to_string(),
                pr_number: Some(7),
                outcome: warden_core::CiWatchOutcome::checks_passed(),
            })
        };

        let (received_json, attempt_result) = tokio::join!(
            receiver,
            deliver_result_or_gate_failed("run-1", &socket_path, attempt)
        );

        assert!(attempt_result.is_ok());
        let message: CiResultMessage =
            warden_core::parse_ci_result_message(&received_json).unwrap();
        assert_eq!(message.pr_number, Some(7));
        assert_eq!(
            message.outcome,
            warden_core::CiWatchOutcome::checks_passed()
        );
    }
}
