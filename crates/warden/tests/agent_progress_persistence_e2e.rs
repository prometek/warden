//! End-to-end coverage for issue #108: agent progress is persisted, so a late attach -- or a replay
//! of a run that already ended -- can show what the agents reported doing, not just the
//! `AgentStarted`/`AgentFinished` brackets around it.
//!
//! Everything here drives the **real `warden` binary** against a **real SQLite database**, with a
//! fake `claude` emitting real `stream-json` lines, and then replays that database through
//! `warden-tui`'s own real reader. The volume scenario is deliberately louder than any unit test
//! would be (`LOUD_LINE_COUNT` assistant turns in one step): the per-step cap only means anything
//! against a run that actually exceeds it.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command as SyncCommand;

use assert_cmd::Command;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use tempfile::TempDir;
use warden::progress_writer::MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP;
use warden_core::{ProgressReplay, RunEvent};
use warden_tui::model::RunModel;

/// How many assistant turns the loud agent emits per invocation -- comfortably past the cap, so the
/// cap is really exercised rather than merely defined.
const LOUD_LINE_COUNT: u32 = MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP + 120;

const SINGLE_AGENT_WORKFLOW: &str = r#"
name: e2e-progress
entry: implementation
steps:
  implementation:
    type: agent
    agent: writer
    on_clean: converged
    on_blocking: implementation
    on_error: failed
"#;

/// A fake `claude --output-format stream-json`: emits a handful of assistant turns (one text block,
/// one `tool_use` block -- exactly the two shapes `ClaudeAdapter::parse_progress_line` translates),
/// commits a file, then closes with the `result` envelope `extract_findings` reads.
const CHATTY_AGENT: &str = r#"#!/bin/sh
set -eu
payload=$(cat)
role=$(printf '%s' "$payload" | sed -n 's/.*"role":"\([^"]*\)".*/\1/p')
i=1
while [ "$i" -le 5 ]; do
  printf '{"type":"assistant","message":{"content":[{"type":"text","text":"step %s of 5"}]}}\n' "$i"
  printf '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo %s"}}]}}\n' "$i"
  i=$(( i + 1 ))
done
echo "$role" > "out-$role.txt"
git add -A
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "$role"
printf '%s\n' '{"result":""}'
"#;

/// Same, but loud enough to blow past the per-step cap. Each turn is uniquely numbered so the
/// persisted sequence can be checked against the emitted one, position by position.
const LOUD_AGENT: &str = r#"#!/bin/sh
set -eu
payload=$(cat)
role=$(printf '%s' "$payload" | sed -n 's/.*"role":"\([^"]*\)".*/\1/p')
i=1
while [ "$i" -le LINE_COUNT ]; do
  printf '{"type":"assistant","message":{"content":[{"type":"text","text":"turn-%s"}]}}\n' "$i"
  i=$(( i + 1 ))
done
echo "$role" > "out-$role.txt"
git add -A
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "$role"
printf '%s\n' '{"result":""}'
"#;

/// A converged run of the real binary, plus the temporary directories that must outlive it.
struct RealRun {
    db_path: PathBuf,
    _home: TempDir,
    _repo: TempDir,
    _agent_home: TempDir,
    _bin: TempDir,
}

fn git(repo: &Path, args: &[&str]) {
    assert!(SyncCommand::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .unwrap()
        .success());
}

fn init_repo() -> TempDir {
    let repo = TempDir::new().unwrap();
    for args in [
        &["init", "--quiet"][..],
        &["config", "user.email", "test@warden.local"],
        &["config", "user.name", "warden-test"],
    ] {
        assert!(SyncCommand::new("git")
            .current_dir(repo.path())
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(repo.path().join("README.md"), "seed\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "--quiet", "-m", "seed"]);
    repo
}

/// Drives the real `warden run` binary with `agent_script` standing in for the `claude` CLI, and
/// returns the real database it wrote.
fn real_converged_run(agent_script: &str) -> RealRun {
    use std::os::unix::fs::PermissionsExt;

    let repo = init_repo();
    let home = TempDir::new().unwrap();
    let agent_home = TempDir::new().unwrap();
    let bin = TempDir::new().unwrap();

    let workflow_dir = repo.path().join(".warden");
    std::fs::create_dir_all(workflow_dir.join("agents")).unwrap();
    std::fs::write(workflow_dir.join("workflow.yaml"), SINGLE_AGENT_WORKFLOW).unwrap();
    std::fs::write(
        workflow_dir.join("agents").join("writer.md"),
        "---\ntools: Read, Write, Edit, Bash\n---\nDo the work.\n",
    )
    .unwrap();

    let script = bin.path().join("claude");
    std::fs::write(&script, agent_script).unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap()
    );

    Command::new(env!("CARGO_BIN_EXE_warden"))
        .env("PATH", path)
        .env("HOME", agent_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "report progress",
            "--warden-home",
            home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("finished: Converged"));

    RealRun {
        db_path: home.path().join("state.db"),
        _home: home,
        _repo: repo,
        _agent_home: agent_home,
        _bin: bin,
    }
}

async fn open(db_path: &Path) -> SqlitePool {
    let options = SqliteConnectOptions::new().filename(db_path);
    SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .unwrap()
}

async fn only_run_id(pool: &SqlitePool) -> String {
    let rows = sqlx::query("SELECT id FROM runs")
        .fetch_all(pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly one run per warden invocation");
    rows[0].get::<String, _>("id")
}

async fn final_state(pool: &SqlitePool, run_id: &str) -> String {
    sqlx::query("SELECT state FROM runs WHERE id = ?")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<String, _>("state")
}

/// Replays a real run's real `events` rows through `warden-tui`'s own reader -- the exact code path
/// a late `warden-tui attach` takes before it switches to the live socket.
async fn replay(db_path: &Path, run_id: &str, progress: ProgressReplay) -> Vec<RunEvent> {
    let pool = warden_tui::db::connect_read_only(db_path).await.unwrap();
    warden_tui::db::list_events_for_run(&pool, run_id, progress)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.event().expect("every row must decode").clone())
        .collect()
}

fn progress_details(events: &[RunEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            RunEvent::AgentProgress { detail, .. } => Some(detail.as_str()),
            _ => None,
        })
        .collect()
}

fn kinds(events: &[RunEvent]) -> Vec<&'static str> {
    events.iter().map(|event| event.kind().as_str()).collect()
}

/// Acceptance criteria 1 and 2, through the real binary: a finished run replayed exposes the
/// progress sequence a live subscriber saw, in publication order -- and only when asked for it.
#[tokio::test]
async fn a_finished_run_replays_its_agent_progress_in_publication_order_when_opted_in() {
    let run = real_converged_run(CHATTY_AGENT);
    let pool = open(&run.db_path).await;
    let run_id = only_run_id(&pool).await;

    let opted_in = replay(&run.db_path, &run_id, ProgressReplay::Included).await;
    assert_eq!(
        progress_details(&opted_in),
        vec![
            "message: step 1 of 5",
            "tool_use: Bash (echo 1)",
            "message: step 2 of 5",
            "tool_use: Bash (echo 2)",
            "message: step 3 of 5",
            "tool_use: Bash (echo 3)",
            "message: step 4 of 5",
            "tool_use: Bash (echo 4)",
            "message: step 5 of 5",
            "tool_use: Bash (echo 5)",
        ],
        "the replayed sequence must be the emitted one, in order, text and tool_use alike"
    );

    let default_replay = replay(&run.db_path, &run_id, ProgressReplay::Excluded).await;
    assert!(
        progress_details(&default_replay).is_empty(),
        "a default attach must stay as cheap as it was before progress was persisted: {:?}",
        kinds(&default_replay)
    );
    assert_eq!(
        kinds(&default_replay),
        kinds(&opted_in)
            .into_iter()
            .filter(|kind| *kind != "agent_progress")
            .collect::<Vec<_>>(),
        "excluding progress must remove exactly that, and leave the rest untouched"
    );
}

/// Acceptance criterion 3's ordering half: every progress event of a step is flushed *before* that
/// step's `AgentFinished` is persisted, so a replay reads a step's progress inside the step, not
/// spilling past its end.
#[tokio::test]
async fn a_steps_progress_is_flushed_before_its_agent_finished_row() {
    let run = real_converged_run(CHATTY_AGENT);
    let pool = open(&run.db_path).await;
    let run_id = only_run_id(&pool).await;

    let replayed = kinds(&replay(&run.db_path, &run_id, ProgressReplay::Included).await);
    let started = replayed
        .iter()
        .position(|kind| *kind == "agent_started")
        .expect("the step must have started");
    let finished = replayed
        .iter()
        .position(|kind| *kind == "agent_finished")
        .expect("the step must have finished");
    let progress_positions: Vec<usize> = replayed
        .iter()
        .enumerate()
        .filter(|(_, kind)| **kind == "agent_progress")
        .map(|(index, _)| index)
        .collect();

    assert!(!progress_positions.is_empty(), "{replayed:?}");
    assert!(
        progress_positions
            .iter()
            .all(|position| *position > started && *position < finished),
        "every progress row must sit strictly between its step's brackets: {replayed:?}"
    );
}

/// Acceptance criterion 5: the volume policy applied to a run that really is loud. The cap holds,
/// what is kept is the *beginning* of the step (its order preserved), and the run converges exactly
/// as it would have without any of this.
#[tokio::test]
async fn a_loud_step_is_capped_without_changing_the_runs_outcome() {
    let script = LOUD_AGENT.replace("LINE_COUNT", &LOUD_LINE_COUNT.to_string());
    let run = real_converged_run(&script);
    let pool = open(&run.db_path).await;
    let run_id = only_run_id(&pool).await;

    assert_eq!(
        final_state(&pool, &run_id).await,
        "converged",
        "the volume policy must never change a run's verdict"
    );

    let replayed = replay(&run.db_path, &run_id, ProgressReplay::Included).await;
    let details = progress_details(&replayed);
    assert_eq!(
        details.len() as u32,
        MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP,
        "a step louder than the cap must persist exactly the cap, never more"
    );
    let expected: Vec<String> = (1..=MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP)
        .map(|turn| format!("message: turn-{turn}"))
        .collect();
    assert_eq!(
        details, expected,
        "what survives the cap is the head of the step, still in publication order"
    );

    let model_events = {
        let mut model = RunModel::new();
        let tui_pool = warden_tui::db::connect_read_only(&run.db_path)
            .await
            .unwrap();
        for entry in
            warden_tui::db::list_events_for_run(&tui_pool, &run_id, ProgressReplay::Included)
                .await
                .unwrap()
        {
            model.apply_history_entry(entry);
        }
        model.events().len()
    };
    assert!(
        model_events > MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP as usize,
        "the TUI model must ingest the capped history whole, not choke on it: {model_events}"
    );
}
