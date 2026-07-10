//! `warden-gated` binary: CLI parsing + dispatch only. All gate logic
//! (read-only re-verification, push, hook installation) lives in the
//! library crate (`src/lib.rs` and friends).

use std::io::Read as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use warden_gated::ci_watcher::{watch_pr, WatchConfig, WatchOutcome};
use warden_gated::gate::verify_and_authorize;
use warden_gated::gh_provider::GhProvider;
use warden_gated::pr_manager::PrHandle;
use warden_gated::verify::GateDecision;
use warden_gated::{bare_repo, db, hook, relay, serve};

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
