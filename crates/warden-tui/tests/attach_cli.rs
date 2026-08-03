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

async fn run_attach_cli_raw(
    run_id: &str,
    db_path: &Path,
    warden_home: &Path,
) -> (std::process::ExitStatus, Vec<String>, String) {
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
    let lines: Vec<String> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    let stderr = String::from_utf8(output.stderr).unwrap();
    (output.status, lines, stderr)
}

async fn run_attach_cli(
    run_id: &str,
    db_path: &Path,
    warden_home: &Path,
) -> (std::process::ExitStatus, Vec<RunEventRecord>) {
    let (status, lines, _stderr) = run_attach_cli_raw(run_id, db_path, warden_home).await;
    let records: Vec<RunEventRecord> = lines
        .iter()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("malformed NDJSON line {line:?}: {error}"))
        })
        .collect();
    (status, records)
}

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
            max_cycles: 5,
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
        tokio::time::sleep(Duration::from_millis(200)).await;
        let line = serde_json::to_string(&live_event_clone).unwrap();
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
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
            max_cycles: 5,
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

    let (status, records) = run_attach_cli("run-1", &db_path, &warden_home).await;

    assert!(status.success(), "warden-tui attach must exit 0");
    let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["e1", "e2"]);
}

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
            max_cycles: 5,
        },
        "2026-07-12T00:00:00+00:00",
    )
    .await;

    let warden_home = dir.path().join("warden_home");
    let runs_dir = warden_home.join("runs");
    tokio::fs::create_dir_all(&runs_dir).await.unwrap();
    let socket_path = resolve_socket_path("run-1", &runs_dir);
    let listener = UnixListener::bind(&socket_path).unwrap();

    let already_historical_event = RunEventRecord {
        id: "e1".to_string(),
        run_id: "run-1".to_string(),
        event: RunEvent::RunStarted {
            intent: "intent".to_string(),
            branch: "main".to_string(),
            max_cycles: 5,
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

/// Issue #107, acceptance criterion 2, through the real `warden-tui` binary: a late attach replays
/// the recorded workflow graph as a *decoded* event, with every declared step -- including one the
/// run never started. If `EventKind::parse` or the payload tag ever drifts, this row silently
/// degrades to `undecodable` and the observer loses the graph; asserting on the model alone would
/// never notice.
#[tokio::test]
async fn attach_cli_replays_the_recorded_workflow_graph_including_never_started_steps() {
    let dir = TempDir::new().unwrap();
    let (db_path, pool) = seeded_db(dir.path()).await;
    insert_event(
        &pool,
        "e1",
        &RunEvent::RunStarted {
            intent: "intent".to_string(),
            branch: "main".to_string(),
            max_cycles: 5,
        },
        "2026-07-12T00:00:00+00:00",
    )
    .await;
    insert_event(
        &pool,
        "e2",
        &RunEvent::WorkflowResolved {
            name: "quality-loop".to_string(),
            entry: 0,
            steps: vec![
                warden_core::WorkflowStepWire {
                    index: 0,
                    id: "implementation".to_string(),
                    kind: "agent".to_string(),
                    on_clean: "verification".to_string(),
                    on_blocking: "implementation".to_string(),
                    on_error: "failed".to_string(),
                    max_cycles: None,
                    captures_evidence: false,
                },
                warden_core::WorkflowStepWire {
                    index: 1,
                    id: "remediation".to_string(),
                    kind: "agent".to_string(),
                    on_clean: "verification".to_string(),
                    on_blocking: "implementation".to_string(),
                    on_error: "failed".to_string(),
                    max_cycles: Some(2),
                    captures_evidence: true,
                },
                warden_core::WorkflowStepWire {
                    index: 2,
                    id: "verification".to_string(),
                    kind: "command".to_string(),
                    on_clean: "converged".to_string(),
                    on_blocking: "remediation".to_string(),
                    on_error: "failed".to_string(),
                    max_cycles: None,
                    captures_evidence: false,
                },
            ],
        },
        "2026-07-12T00:00:01+00:00",
    )
    .await;
    insert_event(
        &pool,
        "e3",
        &RunEvent::AgentStarted {
            role: "implementation".to_string(),
        },
        "2026-07-12T00:00:02+00:00",
    )
    .await;
    insert_event(
        &pool,
        "e4",
        &RunEvent::RunFinished {
            final_state: "failed".to_string(),
        },
        "2026-07-12T00:00:03+00:00",
    )
    .await;

    let warden_home = dir.path().join("warden_home");
    tokio::fs::create_dir_all(warden_home.join("runs"))
        .await
        .unwrap();

    let (status, lines, stderr) = run_attach_cli_raw("run-1", &db_path, &warden_home).await;
    assert!(status.success(), "warden-tui attach must exit 0: {stderr}");

    let parsed: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        parsed.iter().all(|line| line.get("undecodable").is_none()),
        "no row may degrade to undecodable: {parsed:?}"
    );

    let graph = &parsed[1]["event"];
    assert_eq!(graph["kind"], "workflow_resolved");
    assert_eq!(graph["name"], "quality-loop");
    assert_eq!(graph["entry"], 0);
    let steps = graph["steps"].as_array().unwrap();
    assert_eq!(
        steps.len(),
        3,
        "the whole declared graph must survive the replay: {steps:?}"
    );
    assert_eq!(steps[1]["id"], "remediation");
    assert_eq!(
        steps[1]["max_cycles"], 2,
        "a step's own budget must survive the replay"
    );
    assert_eq!(steps[1]["captures_evidence"], true);
    assert_eq!(steps[2]["kind"], "command");
    assert_eq!(steps[2]["on_blocking"], "remediation");

    // Only `implementation` ever started -- the other two are still knowable purely from the graph.
    let started: Vec<&serde_json::Value> = parsed
        .iter()
        .filter(|line| line["event"]["kind"] == "agent_started")
        .collect();
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["event"]["role"], "implementation");
}

#[tokio::test]
async fn attach_cli_headless_surfaces_undecodable_rows_as_tagged_ndjson_lines_and_stderr_warnings()
{
    let dir = TempDir::new().unwrap();
    let (db_path, pool) = seeded_db(dir.path()).await;

    insert_event(
        &pool,
        "event-good-1",
        &RunEvent::RunStarted {
            intent: "intent".to_string(),
            branch: "main".to_string(),
            max_cycles: 5,
        },
        "2026-07-12T00:00:00+00:00",
    )
    .await;

    sqlx::query(
        "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, 'run-1', ?, ?, ?)",
    )
    .bind("event-malformed")
    .bind("cycle_started")
    .bind("{ not json")
    .bind("2026-07-12T00:00:01+00:00")
    .execute(&pool)
    .await
    .unwrap();

    let mismatched_payload =
        serde_json::to_string(&RunEvent::CycleStarted { cycle_number: 1 }).unwrap();
    sqlx::query(
        "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, 'run-1', ?, ?, ?)",
    )
    .bind("event-mismatched")
    .bind("run_finished")
    .bind(mismatched_payload)
    .bind("2026-07-12T00:00:02+00:00")
    .execute(&pool)
    .await
    .unwrap();

    insert_event(
        &pool,
        "event-good-2",
        &RunEvent::CycleStarted { cycle_number: 2 },
        "2026-07-12T00:00:03+00:00",
    )
    .await;

    let warden_home = dir.path().join("warden_home");
    tokio::fs::create_dir_all(warden_home.join("runs"))
        .await
        .unwrap();

    let (status, lines, stderr) = run_attach_cli_raw("run-1", &db_path, &warden_home).await;

    assert!(
        status.success(),
        "warden-tui attach must exit 0 even with undecodable rows in history: {stderr}"
    );

    let parsed: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("malformed NDJSON line {line:?}: {error}"))
        })
        .collect();
    assert_eq!(
        parsed.len(),
        4,
        "every row (good and undecodable) must be represented on stdout: {parsed:?}"
    );

    assert!(parsed[0].get("event").is_some(), "{:?}", parsed[0]);
    assert_eq!(parsed[0]["id"], "event-good-1");

    assert!(parsed[1].get("undecodable").is_some(), "{:?}", parsed[1]);
    assert_eq!(parsed[1]["undecodable"]["id"], "event-malformed");
    assert_eq!(parsed[1]["undecodable"]["event_type"], "cycle_started");
    assert_eq!(
        parsed[1]["undecodable"]["reason"]["kind"],
        "payload_deserialize"
    );

    assert!(parsed[2].get("undecodable").is_some(), "{:?}", parsed[2]);
    assert_eq!(parsed[2]["undecodable"]["id"], "event-mismatched");
    assert_eq!(parsed[2]["undecodable"]["event_type"], "run_finished");
    assert_eq!(parsed[2]["undecodable"]["reason"]["kind"], "kind_mismatch");
    assert_eq!(
        parsed[2]["undecodable"]["reason"]["payload_kind"],
        "cycle_started"
    );

    assert!(parsed[3].get("event").is_some(), "{:?}", parsed[3]);
    assert_eq!(parsed[3]["id"], "event-good-2");

    assert!(stderr.contains("event-malformed"), "{stderr}");
    assert!(stderr.contains("event-mismatched"), "{stderr}");
    assert!(stderr.contains("could not be decoded"), "{stderr}");
}

#[tokio::test]
async fn attach_cli_headless_still_exits_0_when_every_row_in_history_is_undecodable() {
    let dir = TempDir::new().unwrap();
    let (db_path, pool) = seeded_db(dir.path()).await;

    sqlx::query(
        "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, 'run-1', ?, ?, ?)",
    )
    .bind("event-unknown-kind")
    .bind("workflow_step_added")
    .bind(r#"{"kind":"workflow_step_added"}"#)
    .bind("2026-07-12T00:00:00+00:00")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, 'run-1', ?, ?, ?)",
    )
    .bind("event-malformed")
    .bind("cycle_started")
    .bind("{ not json")
    .bind("2026-07-12T00:00:01+00:00")
    .execute(&pool)
    .await
    .unwrap();

    let warden_home = dir.path().join("warden_home");
    tokio::fs::create_dir_all(warden_home.join("runs"))
        .await
        .unwrap();

    let (status, lines, stderr) = run_attach_cli_raw("run-1", &db_path, &warden_home).await;

    assert!(
        status.success(),
        "an all-undecodable history must never crash or exit non-zero: {stderr}"
    );

    let parsed: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("malformed NDJSON line {line:?}: {error}"))
        })
        .collect();
    assert_eq!(parsed.len(), 2, "{parsed:?}");
    assert!(
        parsed.iter().all(|line| line.get("undecodable").is_some()),
        "every line must be tagged undecodable, none silently dropped nor mistaken for a \
         decoded event: {parsed:?}"
    );
    assert_eq!(parsed[0]["undecodable"]["id"], "event-unknown-kind");
    assert_eq!(
        parsed[0]["undecodable"]["reason"]["kind"],
        "unknown_event_type"
    );
    assert_eq!(parsed[1]["undecodable"]["id"], "event-malformed");
}
