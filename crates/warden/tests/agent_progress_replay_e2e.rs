//! Issue #108, the acceptance criteria that only a *running* run can answer.
//!
//! `tests/agent_progress_persistence_e2e.rs` already drives the real `warden` binary and replays
//! what it wrote, but every one of its scenarios inspects a run that has already **ended**. Three of
//! the ticket's claims are not observable that way, and are what this suite adds:
//!
//! - **Criterion 1, literally** ("a late attach replays the *elapsed* progress"): the ticket's own
//!   scenario is attaching to a run still in flight, ten minutes in. Nothing pinned that progress
//!   reaches the table *during* an invocation -- a writer that only wrote at `flush` would satisfy
//!   every finished-run assertion in the existing suite and still show a mid-run attacher nothing.
//! - **Criterion 2 and the NDJSON coherence complaint** ("a finished run replayed exposes the same
//!   progress sequence a live subscriber would have seen"): the existing suite compares a replay to
//!   the sequence the fake agent was *scripted* to emit. This one compares it to what a real
//!   subscriber, attached to the real Event Bus of the real run, actually received.
//! - **Criterion 4, end to end** ("a write failure does not change run state or verdict"): pinned so
//!   far by a unit test on a pool. Here every single progress write of a real run fails, and the run
//!   still has to converge, keep its lifecycle events, and say so on stderr.
//!
//! Plus the *observable* half of criteria 3 and 5: the drop the volume policy causes has to be
//! **logged**, and the log is the only thing an operator ever sees of it.
//!
//! The consumer side uses `warden_tui::attach::attach` -- the function `warden-tui`'s `main.rs`
//! calls one line below argument parsing -- rather than the `warden-tui` binary, because
//! `CARGO_BIN_EXE_warden-tui` does not exist in this package. The NDJSON layer above it is pinned
//! through the real binary in `crates/warden-tui/tests/attach_cli.rs`.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command as SyncCommand, Output, Stdio};
use std::time::{Duration, Instant};

use sqlx::{Row, SqlitePool};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use warden_core::{resolve_socket_path, ProgressReplay, RunEvent};
use warden_tui::model::{HistoryItem, RunModel};

const SINGLE_AGENT_WORKFLOW: &str = r#"
name: e2e-progress-replay
entry: implementation
steps:
  implementation:
    type: agent
    agent: writer
    on_clean: converged
    on_blocking: implementation
    on_error: failed
"#;

/// How many progress lines the gated agent emits before it stops and waits to be released. The
/// attach under test happens in that window: these are the "elapsed" lines criterion 1 is about.
const LINES_BEFORE_GATE: usize = 5;
/// How many it emits after being released -- the ones a subscriber can only get live.
const LINES_AFTER_GATE: usize = 5;

/// A fake `claude --output-format stream-json` that emits [`LINES_BEFORE_GATE`] assistant turns,
/// then **blocks until `GATE_PATH` appears**, then emits [`LINES_AFTER_GATE`] more and finishes
/// clean. The gate is what makes "attach while the agent is mid-invocation" a state the test can
/// reach on purpose instead of by timing.
const GATED_AGENT: &str = r#"#!/bin/sh
set -eu
payload=$(cat)
role=$(printf '%s' "$payload" | sed -n 's/.*"role":"\([^"]*\)".*/\1/p')
i=1
while [ "$i" -le 5 ]; do
  printf '{"type":"assistant","message":{"content":[{"type":"text","text":"early-%s"}]}}\n' "$i"
  i=$(( i + 1 ))
done
while [ ! -f "GATE_PATH" ]; do
  sleep 0.05
done
i=1
while [ "$i" -le 5 ]; do
  printf '{"type":"assistant","message":{"content":[{"type":"text","text":"late-%s"}]}}\n' "$i"
  i=$(( i + 1 ))
done
echo "$role" > "out-$role.txt"
git add -A
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "$role"
printf '%s\n' '{"result":""}'
"#;

/// A quiet agent: five turns, one commit, clean verdict.
const CHATTY_AGENT: &str = r#"#!/bin/sh
set -eu
payload=$(cat)
role=$(printf '%s' "$payload" | sed -n 's/.*"role":"\([^"]*\)".*/\1/p')
i=1
while [ "$i" -le 5 ]; do
  printf '{"type":"assistant","message":{"content":[{"type":"text","text":"turn-%s"}]}}\n' "$i"
  i=$(( i + 1 ))
done
echo "$role" > "out-$role.txt"
git add -A
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "$role"
printf '%s\n' '{"result":""}'
"#;

/// Blocking on its first invocation, clean on its second, and modest in both: the convergence loop
/// re-enters the same step, so the run has two agent invocations to bracket.
const RELOOPING_AGENT: &str = r#"#!/bin/sh
set -eu
payload=$(cat)
role=$(printf '%s' "$payload" | sed -n 's/.*"role":"\([^"]*\)".*/\1/p')
if [ -f "cycle-1-done.txt" ]; then
  cycle=2
else
  cycle=1
fi
i=1
while [ "$i" -le 3 ]; do
  printf '{"type":"assistant","message":{"content":[{"type":"text","text":"cycle-%s-turn-%s"}]}}\n' "$cycle" "$i"
  i=$(( i + 1 ))
done
if [ "$cycle" -eq 1 ]; then
  echo "$role" > "cycle-1-done.txt"
  git add -A
  git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "$role"
  printf '%s\n' '{"result":"{\"source\":\"implementation\",\"severity\":\"blocking\",\"description\":\"one more pass\"}"}'
else
  printf '%s\n' '{"result":""}'
fi
"#;

/// Emits more assistant turns in one invocation than the persistence cap allows, so the cap really
/// fires and really has to be reported.
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

/// A repo, a workflow, and a fake `claude` on `PATH` -- everything one `warden run` needs except
/// the warden home, which is kept separate so two runs can share one database.
struct Workspace {
    repo: TempDir,
    agent_home: TempDir,
    bin: TempDir,
}

impl Workspace {
    fn new(agent_script: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let repo = init_repo();
        let workflow_dir = repo.path().join(".warden");
        std::fs::create_dir_all(workflow_dir.join("agents")).unwrap();
        std::fs::write(workflow_dir.join("workflow.yaml"), SINGLE_AGENT_WORKFLOW).unwrap();
        std::fs::write(
            workflow_dir.join("agents").join("writer.md"),
            "---\ntools: Read, Write, Edit, Bash\n---\nDo the work.\n",
        )
        .unwrap();

        let bin = TempDir::new().unwrap();
        let script = bin.path().join("claude");
        std::fs::write(&script, agent_script).unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        Self {
            repo,
            agent_home: TempDir::new().unwrap(),
            bin,
        }
    }

    fn path_env(&self) -> String {
        format!(
            "{}:{}",
            self.bin.path().display(),
            std::env::var("PATH").unwrap()
        )
    }

    fn warden_args(&self, warden_home: &Path) -> Vec<String> {
        [
            "run",
            "--repo",
            self.repo.path().to_str().unwrap(),
            "--intent",
            "report progress",
            "--warden-home",
            warden_home.to_str().unwrap(),
            "--tool",
            "claude",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }
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

/// Runs the real `warden` binary to completion and hands back its raw output, stderr included --
/// the logs are the assertion target of half this suite.
fn run_warden(warden_home: &Path, workspace: &Workspace) -> Output {
    let output = SyncCommand::new(env!("CARGO_BIN_EXE_warden"))
        .env("PATH", workspace.path_env())
        .env("HOME", workspace.agent_home.path())
        .args(workspace.warden_args(warden_home))
        .output()
        .expect("warden must be runnable");
    assert!(
        output.status.success(),
        "warden run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// Everything the binary emitted, both streams. `warden`'s `init_tracing` builds a
/// `tracing_subscriber::fmt()` subscriber, whose default writer is **stdout**, while the CLI's own
/// direct notices go to stderr; an operator reads the pair, and so does this suite.
fn logs_of(output: &Output) -> String {
    strip_ansi(&format!("{}{}", stdout_of(output), stderr_of(output)))
}

/// `tracing_subscriber::fmt` styles its field separators with SGR escapes, which would split
/// `dropped_over_cap=120` in the middle. Assertions here are about what the line *says*, not how a
/// terminal paints it.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        if chars.next() != Some('[') {
            continue;
        }
        for escaped in chars.by_ref() {
            if escaped.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

async fn open_read_write(db_path: &Path) -> SqlitePool {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    SqlitePoolOptions::new()
        .connect_with(SqliteConnectOptions::new().filename(db_path))
        .await
        .unwrap()
}

/// Every run id in the database, in insertion order.
async fn run_ids(pool: &SqlitePool) -> Vec<String> {
    sqlx::query("SELECT id FROM runs ORDER BY rowid ASC")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<String, _>("id"))
        .collect()
}

async fn run_state(pool: &SqlitePool, run_id: &str) -> String {
    sqlx::query("SELECT state FROM runs WHERE id = ?")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<String, _>("state")
}

/// Replays a run through `warden_tui`'s own read-only reader -- the path a late attach takes.
async fn replay(db_path: &Path, run_id: &str, progress: ProgressReplay) -> Vec<RunEvent> {
    let pool = warden_tui::db::connect_read_only(db_path).await.unwrap();
    let events = warden_tui::db::list_events_for_run(&pool, run_id, progress)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.event().expect("every row must decode").clone())
        .collect();
    pool.close().await;
    events
}

fn progress_details(events: &[RunEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            RunEvent::AgentProgress { detail, .. } => Some(detail.clone()),
            _ => None,
        })
        .collect()
}

fn kinds(events: &[RunEvent]) -> Vec<&'static str> {
    events.iter().map(|event| event.kind().as_str()).collect()
}

/// The progress details a [`RunModel`] holds, in the order it would render (and dump as NDJSON)
/// them.
fn model_progress_details(model: &RunModel) -> Vec<String> {
    model
        .history()
        .into_iter()
        .filter_map(|item| match item {
            HistoryItem::Event(record) => match &record.event {
                RunEvent::AgentProgress { detail, .. } => Some(detail.clone()),
                _ => None,
            },
            HistoryItem::Undecodable(_) => None,
        })
        .collect()
}

/// Waits on a *condition*, never on a duration: polls `check` until it returns `true`, and panics
/// with `what` if it never does. Every wait in this file goes through here, so no assertion depends
/// on how fast a machine happens to be.
async fn wait_until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    const DEADLINE: Duration = Duration::from_secs(60);
    let started = Instant::now();
    while started.elapsed() < DEADLINE {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out after {DEADLINE:?} waiting for: {what}");
}

/// How many `agent_progress` rows the database holds for `run_id` right now. Returns `None` while
/// the database is not yet readable -- `warden` creates and migrates it as it starts.
async fn persisted_progress_count(db_path: &Path, run_id: &str) -> Option<usize> {
    if !db_path.exists() {
        return None;
    }
    let pool = warden_tui::db::connect_read_only(db_path).await.ok()?;
    let count = sqlx::query("SELECT COUNT(*) AS n FROM events WHERE run_id = ? AND event_type = ?")
        .bind(run_id)
        .bind("agent_progress")
        .fetch_one(&pool)
        .await
        .ok()
        .map(|row| row.get::<i64, _>("n") as usize);
    pool.close().await;
    count
}

/// Criteria 1 and 2, and the ticket's NDJSON coherence complaint, against a run that is **still
/// running**.
///
/// A subscriber attaches mid-invocation, after five progress lines have already been published and
/// before the remaining five are: it must reconstruct the whole sequence -- the elapsed part from
/// the table, the rest from the bus. A second observer, attaching only once the run is over, must
/// then see *that same sequence*. Before issue #108 the first observer saw five lines and the second
/// saw none; that difference is precisely what the ticket calls incoherent.
#[tokio::test]
async fn a_mid_run_attach_and_a_post_run_replay_expose_the_same_progress_sequence() {
    let gate = TempDir::new().unwrap();
    let gate_path = gate.path().join("release-the-agent");
    let workspace = Workspace::new(&GATED_AGENT.replace("GATE_PATH", gate_path.to_str().unwrap()));
    let home = TempDir::new().unwrap();
    let db_path = home.path().join("state.db");
    let runs_dir = home.path().join("runs");

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_warden"))
        .env("PATH", workspace.path_env())
        .env("HOME", workspace.agent_home.path())
        .args(workspace.warden_args(home.path()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("warden must be runnable");

    // Drained concurrently so a full pipe can never be what blocks the run under test.
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let stderr_reader = child.stderr.take().unwrap();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr_reader).lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            collected.push_str(&line);
            collected.push('\n');
        }
        collected
    });

    // `warden run` announces the run id on stdout before the first agent starts.
    let mut run_id = None;
    while let Some(line) = stdout.next_line().await.unwrap() {
        if let Some(rest) = line.strip_prefix("run ") {
            if let Some(id) = rest.strip_suffix(" started") {
                run_id = Some(id.to_string());
                break;
            }
        }
    }
    let run_id = run_id.expect("warden run must announce its run id on stdout");
    let stdout_task =
        tokio::spawn(async move { while stdout.next_line().await.ok().flatten().is_some() {} });

    // The agent is now blocked on the gate, having published exactly LINES_BEFORE_GATE turns.
    // Waiting for them to be *on disk* is itself the assertion criterion 1 needs: progress reaches
    // the table while the invocation is still running, not only when it ends.
    wait_until(
        "the elapsed progress of a still-running invocation to be persisted",
        || async {
            persisted_progress_count(&db_path, &run_id)
                .await
                .unwrap_or(0)
                >= LINES_BEFORE_GATE
        },
    )
    .await;

    let live_pool = warden_tui::db::connect_read_only(&db_path).await.unwrap();
    let socket_path = resolve_socket_path(&run_id, &runs_dir);
    let mut attachment =
        warden_tui::attach::attach(&live_pool, &run_id, &socket_path, ProgressReplay::Included)
            .await
            .expect("attaching to a live run must succeed");
    let replayed_on_attach = model_progress_details(&attachment.model);
    assert_eq!(
        replayed_on_attach.len(),
        LINES_BEFORE_GATE,
        "a late attach must replay the progress that already elapsed, not an empty history: \
         {replayed_on_attach:?}"
    );
    let mut live = attachment
        .live
        .take()
        .expect("a run still in flight must have a live Event Bus");

    // Only now is the agent allowed to finish, so everything that follows can only reach this
    // subscriber over the bus.
    std::fs::write(&gate_path, b"go\n").unwrap();

    while let Some(record) = tokio::time::timeout(Duration::from_secs(60), live.recv())
        .await
        .expect("the live stream must close when the run ends")
    {
        attachment.model.apply(record);
    }
    let live_observer_saw = model_progress_details(&attachment.model);
    live_pool.close().await;

    let status = tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .expect("warden must exit once the agent is released")
        .unwrap();
    let stderr = stderr_task.await.unwrap();
    stdout_task.await.unwrap();
    assert!(status.success(), "warden run must succeed: {stderr}");

    let expected: Vec<String> = (1..=LINES_BEFORE_GATE)
        .map(|index| format!("message: early-{index}"))
        .chain((1..=LINES_AFTER_GATE).map(|index| format!("message: late-{index}")))
        .collect();
    assert_eq!(
        live_observer_saw, expected,
        "the mid-run subscriber must end up with the whole sequence: the elapsed lines from the \
         table, the rest from the bus, in publication order"
    );

    let late_replay = progress_details(&replay(&db_path, &run_id, ProgressReplay::Included).await);
    assert_eq!(
        late_replay, live_observer_saw,
        "a replay of the finished run must expose exactly what the live subscriber saw -- the \
         incoherence issue #108 exists to remove"
    );

    let default_replay = replay(&db_path, &run_id, ProgressReplay::Excluded).await;
    assert!(
        progress_details(&default_replay).is_empty(),
        "the documented default still excludes progress from a replay: {:?}",
        kinds(&default_replay)
    );
}

/// Criterion 4, end to end: **every** progress write of a real run fails, and the run must not
/// notice.
///
/// The failure is injected where a real one would strike -- in SQLite, on the `events` insert
/// itself -- by a trigger that aborts every `agent_progress` row. A trigger, not a mock: nothing in
/// `warden` is aware of it, so what is exercised is the real writer's real error path.
#[tokio::test]
async fn a_run_whose_every_progress_write_fails_still_converges_with_its_history_intact() {
    let home = TempDir::new().unwrap();
    let db_path = home.path().join("state.db");

    // A first, undisturbed run: it creates and migrates the database, and its own persisted
    // progress is the control that proves the second run's emptiness comes from the injected
    // failure and nothing else.
    let control_workspace = Workspace::new(CHATTY_AGENT);
    run_warden(home.path(), &control_workspace);

    let pool = open_read_write(&db_path).await;
    sqlx::query(
        "CREATE TRIGGER reject_agent_progress BEFORE INSERT ON events \
         WHEN NEW.event_type = 'agent_progress' \
         BEGIN SELECT RAISE(ABORT, 'injected progress write failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let failing_workspace = Workspace::new(CHATTY_AGENT);
    let output = run_warden(home.path(), &failing_workspace);
    assert!(
        stdout_of(&output).contains("finished: Converged"),
        "a progress write failure must not change the run's verdict: {}",
        stdout_of(&output)
    );

    let pool = open_read_write(&db_path).await;
    let ids = run_ids(&pool).await;
    assert_eq!(ids.len(), 2, "one run per warden invocation");
    let (control_id, failing_id) = (ids[0].clone(), ids[1].clone());
    assert_eq!(
        run_state(&pool, &failing_id).await,
        "converged",
        "a progress write failure must not change the run's persisted state"
    );
    pool.close().await;

    let control = replay(&db_path, &control_id, ProgressReplay::Included).await;
    assert_eq!(
        progress_details(&control).len(),
        5,
        "control: with no trigger in the way, the same agent's progress is persisted"
    );

    let failing = replay(&db_path, &failing_id, ProgressReplay::Included).await;
    assert!(
        progress_details(&failing).is_empty(),
        "precondition of this test: not one progress row may have survived the trigger"
    );
    assert_eq!(
        kinds(&failing),
        kinds(&control)
            .into_iter()
            .filter(|kind| *kind != "agent_progress")
            .collect::<Vec<_>>(),
        "losing every progress row must cost the run nothing else: same lifecycle history, same \
         order, same verdict"
    );

    assert!(
        logs_of(&output).contains("failed to persist a batch of agent progress events"),
        "a lost batch must be journalled, never swallowed: {}",
        logs_of(&output)
    );
}

/// Criteria 3 and 5, at the only place an operator can see them: `warden`'s own stderr.
///
/// The cap is what makes the volume policy real, and dropping is what the cap *does*. The unit
/// suite pins the counters behind it; what is pinned here is that a run louder than the cap says so
/// out loud -- once, with its numbers -- instead of silently truncating a user's replay.
#[tokio::test]
async fn a_run_that_exceeds_the_cap_reports_the_drop_on_stderr_once() {
    let cap = warden::progress_writer::MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION;
    let over_cap = cap + 120;
    let workspace = Workspace::new(&LOUD_AGENT.replace("LINE_COUNT", &over_cap.to_string()));
    let home = TempDir::new().unwrap();

    let output = run_warden(home.path(), &workspace);
    let logs = logs_of(&output);

    assert_eq!(
        logs.matches("per-invocation cap on persisted agent progress reached")
            .count(),
        1,
        "the cap must be reported exactly once per invocation, not once per dropped event: \
         {logs}"
    );
    assert_eq!(
        logs.matches("agent progress events were dropped during this agent invocation")
            .count(),
        1,
        "the end-of-invocation summary must be emitted once: {logs}"
    );
    assert!(
        logs.contains("dropped_over_cap=120"),
        "the summary must carry how much was dropped, not merely that something was: {logs}"
    );
    assert!(
        stdout_of(&output).contains("finished: Converged"),
        "a capped run must converge exactly as an uncapped one would"
    );
}

/// The flush guarantee, on the run shape that can actually break it: a step the convergence loop
/// re-enters.
///
/// One invocation's progress must be written before that invocation's `agent_finished`. With a
/// single invocation a missing flush is masked -- the run ends and everything lands anyway. With
/// two, cycle 1's queued progress would surface *after* cycle 1's `agent_finished`, and a replay
/// would attribute it to the wrong pass.
#[tokio::test]
async fn each_invocations_progress_is_written_inside_its_own_brackets_when_a_step_reloops() {
    let workspace = Workspace::new(RELOOPING_AGENT);
    let home = TempDir::new().unwrap();
    let db_path = home.path().join("state.db");

    let output = run_warden(home.path(), &workspace);
    assert!(
        stdout_of(&output).contains("finished: Converged"),
        "cycle 1 blocks, cycle 2 comes back clean: {}",
        stdout_of(&output)
    );

    let pool = open_read_write(&db_path).await;
    let run_id = run_ids(&pool).await.remove(0);
    pool.close().await;

    let replayed = replay(&db_path, &run_id, ProgressReplay::Included).await;
    let bracketed: Vec<&'static str> = kinds(&replayed)
        .into_iter()
        .filter(|kind| matches!(*kind, "agent_started" | "agent_progress" | "agent_finished"))
        .collect();

    let mut expected: Vec<&str> = Vec::new();
    for _ in 0..2 {
        expected.push("agent_started");
        expected.extend(["agent_progress"; 3]);
        expected.push("agent_finished");
    }
    assert_eq!(
        bracketed,
        expected,
        "each invocation's progress must be flushed inside its own agent_started/agent_finished \
         bracket: {:?}",
        kinds(&replayed)
    );

    assert_eq!(
        progress_details(&replayed),
        vec![
            "message: cycle-1-turn-1",
            "message: cycle-1-turn-2",
            "message: cycle-1-turn-3",
            "message: cycle-2-turn-1",
            "message: cycle-2-turn-2",
            "message: cycle-2-turn-3",
        ],
        "and each pass keeps its own lines, in publication order"
    );
}
