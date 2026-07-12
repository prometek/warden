//! `warden-tui` binary: CLI parsing + terminal setup/dispatch only. All
//! actual logic (attach/replay/live merge, capability detection, evidence
//! rendering) lives in the library crate.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use ratatui_image::picker::Picker;
use tokio::sync::mpsc;
use warden_core::RunEventRecord;
use warden_tui::attach::{attach, Attachment};
use warden_tui::capabilities::GraphicsCapability;
use warden_tui::{capabilities, db, subscriber, ui};

#[derive(Parser)]
#[command(
    name = "warden-tui",
    version,
    about = "Read-only run monitor: replays a run's history then follows it live (ADR-0008)"
)]
struct Cli {
    /// Increase log verbosity (-v, -vv, -vvv). Logs go to stderr, never
    /// stdout, so they never corrupt the headless NDJSON dump mode.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Attaches to a run: replays its full `events` history, then follows
    /// its Event Bus live if it's still running -- no gap between the two
    /// (Architecture.md §5.4). Renders a full-screen TUI on a real
    /// terminal; on a non-terminal stdout (piped/redirected), streams each
    /// event as one NDJSON line instead.
    Attach {
        /// The run id to attach to (as printed by `warden run`).
        #[arg(long)]
        run_id: String,

        /// `warden`'s SQLite database, opened read-only. Defaults to
        /// `<warden-home>/state.db`.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        db: Option<PathBuf>,

        /// Warden's state directory, used to locate the database and the
        /// run's Event Bus socket. Defaults to `~/.warden`.
        #[arg(long, value_parser = clap::value_parser!(PathBuf))]
        warden_home: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Commands::Attach {
            run_id,
            db,
            warden_home,
        } => attach_cmd(run_id, db, warden_home).await,
    }
}

async fn attach_cmd(
    run_id: String,
    db_path: Option<PathBuf>,
    warden_home: Option<PathBuf>,
) -> anyhow::Result<()> {
    let warden_home = warden_home.unwrap_or(default_warden_home()?);
    let db_path = db_path.unwrap_or_else(|| warden_home.join("state.db"));
    let socket_path = subscriber::resolve_socket_path(&run_id, &warden_home.join("runs"));

    let pool = db::connect_read_only(&db_path)
        .await
        .context("failed to open warden's database read-only")?;
    let attachment = attach(&pool, &run_id, &socket_path)
        .await
        .context("failed to attach to run")?;

    if std::io::stdout().is_terminal() {
        run_tui(attachment).await
    } else {
        run_headless(attachment).await
    }
}

/// Streams every event as one line of NDJSON to stdout: history first, then
/// live as it arrives. Used automatically when stdout isn't a terminal
/// (piped/redirected) -- also what makes this crate's core replay/live
/// behaviour scriptable and end-to-end testable without a real PTY.
async fn run_headless(mut attachment: Attachment) -> anyhow::Result<()> {
    for record in attachment.model.events() {
        println!("{}", serde_json::to_string(record)?);
    }
    if let Some(mut live) = attachment.live.take() {
        while let Some(record) = live.recv().await {
            // A live record can duplicate one already folded into history by
            // `attach()`'s own best-effort drain (see `attach.rs`'s module
            // docs on the "subscribe before querying history" race) --
            // `RunModel::apply`'s id-based dedup is the single source of
            // truth for "is this actually new", so it must gate what gets
            // printed here exactly the way it gates what the interactive
            // `app_loop` renders. Printing straight off the channel without
            // going through the model first would print duplicates.
            if attachment.model.apply(record.clone()) {
                println!("{}", serde_json::to_string(&record)?);
            }
        }
    }
    Ok(())
}

/// Runs the full-screen ratatui app until the user quits (`q`/`Esc`) or the
/// run's live channel closes and no more input is read. Terminal
/// setup/teardown is paired so the error path always restores the user's
/// shell, even if the app loop itself returns an error.
async fn run_tui(attachment: Attachment) -> anyhow::Result<()> {
    let mut terminal = setup_terminal()?;

    // Must run after entering the alternate screen but before reading
    // terminal events, per `ratatui_image::picker::Picker::from_query_stdio`'s
    // own documented contract (ADR-0010). Kept alive for the whole app loop
    // (not dropped) and threaded into `ui::draw`: it's what makes an inline
    // `EvidenceCaptured` image actually reach the screen (acceptance
    // criterion 3), not just get detected and discarded.
    let (capability, picker) = capabilities::detect();
    tracing::info!(?capability, "detected terminal graphics capability");

    let result = app_loop(&mut terminal, attachment, capability, picker.as_ref()).await;

    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    Ok(terminal)
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> anyhow::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

async fn app_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mut attachment: Attachment,
    capability: GraphicsCapability,
    picker: Option<&Picker>,
) -> anyhow::Result<()> {
    // Crossterm's blocking event reader runs on its own OS thread and
    // forwards decoded events over a channel -- keeps this async loop free
    // to `select!` between terminal input and live run events without
    // either one blocking the other.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if input_tx.send(event).is_err() {
                break;
            }
        }
    });

    terminal.draw(|frame| ui::draw(frame, &attachment.model, capability, picker))?;

    loop {
        tokio::select! {
            input = input_rx.recv() => {
                match input {
                    Some(event) if is_quit(&event) => return Ok(()),
                    Some(_) => {}
                    None => return Ok(()), // input thread ended
                }
            }
            record = recv_live(&mut attachment.live) => {
                match record {
                    // The interactive view doesn't need to distinguish a
                    // genuinely new event from a duplicate the way the
                    // headless dump does (`run_headless`) -- every applied
                    // event, new or not, simply results in a redraw of
                    // whatever the model currently holds.
                    Some(record) => { attachment.model.apply(record); }
                    None => attachment.live = None, // run ended; stop selecting on it
                }
            }
        }

        terminal.draw(|frame| ui::draw(frame, &attachment.model, capability, picker))?;
    }
}

/// Awaits the next live event, or never resolves if the run has no (or no
/// longer has) a live channel -- lets `tokio::select!` cleanly fall through
/// to the input branch only, without a busy-poll.
async fn recv_live(
    live: &mut Option<mpsc::UnboundedReceiver<RunEventRecord>>,
) -> Option<RunEventRecord> {
    match live {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn is_quit(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key)
            if key.kind == KeyEventKind::Press
                && (key.code == KeyCode::Char('q') || key.code == KeyCode::Esc)
    )
}

fn default_warden_home() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set; pass --warden-home explicitly")?;
    if home.trim().is_empty() {
        bail!("HOME is empty; pass --warden-home explicitly");
    }
    Ok(PathBuf::from(home).join(".warden"))
}

fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("warden_tui={level}")));
    // stderr, never stdout: stdout is reserved for the headless NDJSON dump
    // (`run_headless`) so logs never corrupt it when piped.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
