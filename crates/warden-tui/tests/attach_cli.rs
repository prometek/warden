//! End-to-end tests driving the actual `warden-tui` binary
//! (`warden-tui attach --run-id ... --db ... --warden-home ...`), not the
//! internal `attach()`/`RunModel` APIs directly -- issue #8 acceptance
//! criterion 1: "a late attach on an already-running run shows the full
//! past history, then switches to the live stream with no gap and no
//! duplicated events."
//!
//! `main.rs::run_headless` (used automatically whenever stdout isn't a
//! terminal, exactly the case for a piped `Command::output()` here) is what
//! makes this reachable without a real PTY -- see its own doc comment.
//! These tests stand up a bare `UnixListener` to play the Event Bus's role
//! ourselves (same technique `src/attach.rs`'s own unit tests already use),
//! rather than depending on the `warden` crate's `EventBus`/orchestrator
//! (this crate deliberately never depends on `warden`'s I/O code, see
//! `src/db.rs`'s module docs) -- but unlike those unit tests, everything
//! here goes through the real `warden-tui attach` subprocess.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use warden_core::{RunEvent, RunEventRecord};
use warden_tui::subscriber::resolve_socket_path;

/// Mirrors `src/attach.rs`'s own test fixture: a real SQLite file with the
/// two tables `warden-tui` reads, built with a plain `CREATE TABLE` rather
/// than `warden`'s migrations (this crate must never depend on those).
async fn seeded_db(dir: &Path) -> (std::path::PathBuf, SqlitePool) {
    let db_path = dir.join("state.db");
    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE runs (
            id TEXT PRIMARY KEY, intent TEXT NOT NULL, branch TEXT NOT NULL,
            state TEXT NOT NULL, max_review_cycles INTEGER NOT NULL,
            max_test_cycles INTEGER NOT NULL, current_review_cycle INTEGER NOT NULL,
            current_test_cycle INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE events (
            id TEXT PRIMARY KEY, run_id TEXT NOT NULL, event_type TEXT NOT NULL,
            payload_json TEXT NOT NULL, created_at TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO runs (id, intent, branch, state, max_review_cycles, max_test_cycles, current_review_cycle, current_test_cycle) VALUES ('run-1', 'intent', 'main', 'coder_running', 5, 5, 1, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    (db_path, pool)
}

async fn insert_event(pool: &SqlitePool, id: &str, event: &RunEvent, created_at: &str) {
    let event_type = event.kind().as_str();
    let payload_json = serde_json::to_string(event).unwrap();
    sqlx::query(
        "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, 'run-1', ?, ?, ?)",
    )
    .bind(id)
    .bind(event_type)
    .bind(payload_json)
    .bind(created_at)
    .execute(pool)
    .await
    .unwrap();
}

/// Spawns `warden-tui attach` against `db_path`/`warden_home`/`run_id`,
/// waits for it to exit (via a blocking-thread `wait_with_output`, so the
/// concurrently spawned mock Event Bus task in the test can keep making
/// progress on the same runtime instead of the whole thread blocking on the
/// child), and returns its parsed stdout as one [`RunEventRecord`] per
/// NDJSON line, in the order printed.
async fn run_attach_cli(
    run_id: &str,
    db_path: &Path,
    warden_home: &Path,
) -> (std::process::ExitStatus, Vec<RunEventRecord>) {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_warden-tui"));
    command
        .args(["attach", "--run-id", run_id])
        .arg("--db")
        .arg(db_path)
        .arg("--warden-home")
        .arg(warden_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command.spawn().expect("spawn warden-tui attach");
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || child.wait_with_output()),
    )
    .await
    .expect("warden-tui attach did not exit in time")
    .expect("spawn_blocking join")
    .expect("wait_with_output");

    let stdout = String::from_utf8(output.stdout).unwrap();
    let records: Vec<RunEventRecord> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("malformed NDJSON line {line:?}: {error}"))
        })
        .collect();
    (output.status, records)
}

/// Acceptance criterion 1 (issue #8), happy path: history is replayed in
/// full, then the live event arriving after attach still shows up, with no
/// gap and no duplicate.
#[tokio::test]
async fn attach_cli_replays_full_history_then_streams_live_events_with_no_gap() {
    let dir = TempDir::new().unwrap();
    let (db_path, pool) = seeded_db(dir.path()).await;
    insert_event(
        &pool,
        "e1",
        &RunEvent::RunStarted {
            intent: "intent".to_string(),
            branch: "main".to_string(),
            max_review_cycles: 5,
            max_test_cycles: 5,
        },
        "2026-07-12T00:00:00+00:00",
    )
    .await;
    insert_event(
        &pool,
        "e2",
        &RunEvent::CycleStarted { cycle_number: 1 },
        "2026-07-12T00:00:01+00:00",
    )
    .await;

    let warden_home = dir.path().join("warden_home");
    let runs_dir = warden_home.join("runs");
    tokio::fs::create_dir_all(&runs_dir).await.unwrap();
    let socket_path = resolve_socket_path("run-1", &runs_dir);
    let listener = UnixListener::bind(&socket_path).unwrap();

    let live_event = RunEventRecord {
        id: "e3".to_string(),
        run_id: "run-1".to_string(),
        event: RunEvent::CycleStarted { cycle_number: 2 },
        created_at: "2026-07-12T00:00:02+00:00".to_string(),
    };
    let live_event_clone = live_event.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _addr) = listener.accept().await.unwrap();
        // Long enough that `attach()`'s own non-blocking history/live drain
        // has certainly already returned to the caller by the time this
        // event is sent -- this is genuinely a *live*-only event, not one
        // racing the replay boundary (see the dedup test below for that
        // case).
        tokio::time::sleep(Duration::from_millis(200)).await;
        let line = serde_json::to_string(&live_event_clone).unwrap();
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        // Drop the connection shortly after so the CLI's headless loop sees
        // EOF and exits instead of hanging forever.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let (status, records) = run_attach_cli("run-1", &db_path, &warden_home).await;
    server.await.unwrap();

    assert!(status.success(), "warden-tui attach must exit 0");
    let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["e1", "e2", "e3"],
        "full history must be replayed before the live event, in order, with nothing lost or duplicated"
    );
}

/// Acceptance criterion 1 (issue #8), history-only path: attaching to a run
/// whose Event Bus socket doesn't exist (already finished, or never live)
/// must still replay its full history through the CLI and exit cleanly --
/// no hang waiting on a live stream that will never arrive.
#[tokio::test]
async fn attach_cli_to_a_finished_run_prints_history_only_and_exits() {
    let dir = TempDir::new().unwrap();
    let (db_path, pool) = seeded_db(dir.path()).await;
    insert_event(
        &pool,
        "e1",
        &RunEvent::RunStarted {
            intent: "intent".to_string(),
            branch: "main".to_string(),
            max_review_cycles: 5,
            max_test_cycles: 5,
        },
        "2026-07-12T00:00:00+00:00",
    )
    .await;
    insert_event(
        &pool,
        "e2",
        &RunEvent::RunFinished {
            final_state: "converged".to_string(),
        },
        "2026-07-12T00:00:01+00:00",
    )
    .await;

    let warden_home = dir.path().join("warden_home");
    tokio::fs::create_dir_all(warden_home.join("runs"))
        .await
        .unwrap();
    // Deliberately no socket bound at all -- the run has already finished.

    let (status, records) = run_attach_cli("run-1", &db_path, &warden_home).await;

    assert!(status.success(), "warden-tui attach must exit 0");
    let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["e1", "e2"]);
}

/// Acceptance criterion 1 (issue #8), the exact race the "subscribe before
/// querying history" ordering (`src/attach.rs`) is meant to guard against:
/// an event that is *both* already recorded in `events` (so the history
/// query returns it) *and* still in flight on the live Event Bus connection
/// when `attach()`'s own best-effort drain runs. `attach()`'s in-process
/// `RunModel` correctly dedupes this by id -- but `main.rs::run_headless`
/// (the path this CLI test exercises) prints history from the model and
/// then prints *every* subsequent live message unconditionally, without
/// ever re-applying it to the model. Deterministically reproduced here by
/// delaying the live delivery of an id that is already in history, rather
/// than relying on real race timing.
#[tokio::test]
async fn attach_cli_does_not_duplicate_an_event_that_is_both_history_and_delayed_live() {
    let dir = TempDir::new().unwrap();
    let (db_path, pool) = seeded_db(dir.path()).await;
    insert_event(
        &pool,
        "e1",
        &RunEvent::RunStarted {
            intent: "intent".to_string(),
            branch: "main".to_string(),
            max_review_cycles: 5,
            max_test_cycles: 5,
        },
        "2026-07-12T00:00:00+00:00",
    )
    .await;

    let warden_home = dir.path().join("warden_home");
    let runs_dir = warden_home.join("runs");
    tokio::fs::create_dir_all(&runs_dir).await.unwrap();
    let socket_path = resolve_socket_path("run-1", &runs_dir);
    let listener = UnixListener::bind(&socket_path).unwrap();

    // Same id/content as the history row above: simulates an event whose
    // live delivery over the socket is slow enough to miss `attach()`'s
    // synchronous, non-blocking drain (which runs essentially immediately
    // after subscribing -- well under the delay below on any normal
    // machine), while a genuinely new event ("e2") follows it.
    let already_historical_event = RunEventRecord {
        id: "e1".to_string(),
        run_id: "run-1".to_string(),
        event: RunEvent::RunStarted {
            intent: "intent".to_string(),
            branch: "main".to_string(),
            max_review_cycles: 5,
            max_test_cycles: 5,
        },
        created_at: "2026-07-12T00:00:00+00:00".to_string(),
    };
    let new_live_event = RunEventRecord {
        id: "e2".to_string(),
        run_id: "run-1".to_string(),
        event: RunEvent::CycleStarted { cycle_number: 1 },
        created_at: "2026-07-12T00:00:01+00:00".to_string(),
    };
    let server = tokio::spawn(async move {
        let (mut stream, _addr) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        for record in [&already_historical_event, &new_live_event] {
            let line = serde_json::to_string(record).unwrap();
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.write_all(b"\n").await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let (status, records) = run_attach_cli("run-1", &db_path, &warden_home).await;
    server.await.unwrap();

    assert!(status.success(), "warden-tui attach must exit 0");
    let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["e1", "e2"],
        "issue #8 acceptance criterion 1 (\"no duplicated events\") is violated if e1 \
         appears twice -- got: {ids:?}"
    );
}
