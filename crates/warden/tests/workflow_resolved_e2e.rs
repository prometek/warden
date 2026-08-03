//! End-to-end coverage for issue #107: a run publishes its *resolved workflow graph* once, at
//! start, so an observer -- including one attaching long after the run ended -- learns what is
//! still to do and why the run reboucles, not just what already happened.
//!
//! Everything here drives the **real `warden` binary** against a **real SQLite database** and then
//! replays that database through `warden-tui`'s own real reader (`warden_tui::db` ->
//! `warden_tui::model::RunModel`). Nothing on the publication path is mocked: a unit test that
//! hand-feeds `RunEvent::WorkflowResolved` into a model proves the model, never that a run emits
//! the event at all.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command as SyncCommand;

use assert_cmd::Command;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use tempfile::TempDir;
use warden_core::RunEvent;
use warden_tui::model::{
    DeclaredStep, ResolvedWorkflow, RunModel, StepRuntimeStatus, WorkflowGraph,
};

/// A deliberately non-linear graph: `implementation` loops back onto itself, `verification` is a
/// `command` step (not an agent) and its blocking edge points *backwards* to `remediation`, which
/// this run never reaches because the command always succeeds. Ids are chosen so the declaration
/// order in `Workflow::steps` (alphabetical -- `parse_yaml` collects into a `BTreeMap`) differs
/// from the execution order, pinning that `index` is the workflow's own index and not a
/// "first executed" counter.
const GRAPH_WORKFLOW: &str = r#"
name: e2e-graph
entry: implementation
steps:
  implementation:
    type: agent
    agent: writer
    on_clean: verification
    on_blocking: implementation
    on_error: failed
  remediation:
    type: agent
    agent: fixer
    on_clean: verification
    on_blocking: implementation
    on_error: failed
    max_cycles: 2
    evidence: true
  verification:
    type: command
    run: test -f README.md
    on_clean: converged
    on_blocking: remediation
    on_error: failed
"#;

/// Same shape, but `review` raises one blocking finding on its first pass, so the run really does
/// reboucle back through `implementation` (4 cycles). `quarantine` is reachable only via `review`'s
/// error edge and is never executed.
const REBOUCLE_WORKFLOW: &str = r#"
name: reboucle-graph
entry: implementation
steps:
  implementation:
    type: agent
    agent: writer
    on_clean: review
    on_blocking: implementation
    on_error: failed
  quarantine:
    type: command
    run: 'true'
    on_clean: converged
    on_blocking: implementation
    on_error: failed
  review:
    type: agent
    agent: reviewer
    on_clean: converged
    on_blocking: implementation
    on_error: quarantine
    max_cycles: 4
"#;

/// A fake `claude`: commits a file so the step produces a diff, then reports "no findings".
const ALWAYS_CLEAN_AGENT: &str = r#"#!/bin/sh
set -eu
payload=$(cat)
role=$(printf '%s' "$payload" | sed -n 's/.*"role":"\([^"]*\)".*/\1/p')
counter="$HOME/count-$role"
n=1
if [ -f "$counter" ]; then n=$(( $(cat "$counter") + 1 )); fi
printf '%s' "$n" > "$counter"
echo "$role-$n" > "out-$role-$n.txt"
git add -A
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "$role-$n"
printf '%s\n' '{"result":""}'
"#;

/// A fake `claude` whose `review` step raises exactly one blocking finding, the first time it runs
/// -- the real trigger for a backward `on_blocking` transition.
const BLOCKS_ONCE_AGENT: &str = r#"#!/bin/sh
set -eu
payload=$(cat)
role=$(printf '%s' "$payload" | sed -n 's/.*"role":"\([^"]*\)".*/\1/p')
counter="$HOME/count-$role"
n=1
if [ -f "$counter" ]; then n=$(( $(cat "$counter") + 1 )); fi
printf '%s' "$n" > "$counter"
if [ "$role" = "review" ]; then
  if [ "$n" = "1" ]; then
    printf '%s\n' '{"result":"{\"source\":\"review\",\"severity\":\"blocking\",\"description\":\"needs work\",\"action\":\"fix it\"}"}'
  else
    printf '%s\n' '{"result":""}'
  fi
  exit 0
fi
echo "$role-$n" > "out-$role-$n.txt"
git add -A
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "$role-$n"
printf '%s\n' '{"result":""}'
"#;

/// The entry step fails outright, so the run dies before reaching anything else. This is the case
/// the ticket exists for: with only a retrospective tree, an observer of this run learns almost
/// nothing about what the workflow was even supposed to do.
const FAILING_WORKFLOW: &str = r#"
name: dies-early
entry: gate
steps:
  gate:
    type: command
    run: 'false'
    on_clean: publish
    on_blocking: failed
    on_error: failed
  publish:
    type: command
    run: 'true'
    on_clean: converged
    on_blocking: failed
    on_error: failed
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

fn real_converged_run(workflow: &str, agents: &[&str], agent_script: &str) -> RealRun {
    real_run(workflow, agents, agent_script, "finished: Converged")
}

/// Drives the real `warden run` binary over `workflow`, with `agent_script` standing in for the
/// `claude` CLI, and returns the real database it wrote. `expected_outcome` is asserted on stdout
/// so a scenario that silently stops meaning what it claims fails here rather than downstream.
fn real_run(
    workflow: &str,
    agents: &[&str],
    agent_script: &str,
    expected_outcome: &str,
) -> RealRun {
    use std::os::unix::fs::PermissionsExt;

    let repo = init_repo();
    let home = TempDir::new().unwrap();
    let agent_home = TempDir::new().unwrap();
    let bin = TempDir::new().unwrap();

    let workflow_dir = repo.path().join(".warden");
    std::fs::create_dir_all(workflow_dir.join("agents")).unwrap();
    std::fs::write(workflow_dir.join("workflow.yaml"), workflow).unwrap();
    for agent in agents {
        std::fs::write(
            workflow_dir.join("agents").join(format!("{agent}.md")),
            "---\ntools: Read, Write, Edit, Bash\n---\nDo the work.\n",
        )
        .unwrap();
    }

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
            "observe the graph",
            "--warden-home",
            home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(expected_outcome));

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

/// The real `events` table, oldest first, in exactly the order `warden-tui` replays it.
async fn event_rows(pool: &SqlitePool, run_id: &str) -> Vec<(String, serde_json::Value)> {
    sqlx::query(
        "SELECT event_type, payload_json FROM events WHERE run_id = ? \
         ORDER BY created_at ASC, id ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row.get::<String, _>("event_type"),
            serde_json::from_str(&row.get::<String, _>("payload_json")).unwrap(),
        )
    })
    .collect()
}

/// Replays a real run's real `events` rows through `warden-tui`'s own reader and model -- the exact
/// code path a late `warden-tui attach` takes before it switches to the live socket.
async fn replay_as_late_attach(db_path: &Path, run_id: &str) -> RunModel {
    let pool = warden_tui::db::connect_read_only(db_path).await.unwrap();
    let history = warden_tui::db::list_events_for_run(&pool, run_id)
        .await
        .unwrap();
    let mut model = RunModel::new();
    for entry in history {
        model.apply_history_entry(entry);
    }
    model
}

fn declared<'a>(graph: &'a ResolvedWorkflow, id: &str) -> &'a DeclaredStep {
    graph
        .steps
        .iter()
        .find(|step| step.id == id)
        .unwrap_or_else(|| panic!("step {id:?} missing from the resolved graph: {graph:?}"))
}

/// Acceptance criterion 1, through the real binary: exactly one `workflow_resolved` row, sitting
/// immediately after `run_started` and *before* any step transition, carrying the whole declared
/// graph -- including the step this run never reaches.
#[tokio::test]
async fn a_real_run_records_its_resolved_workflow_graph_once_before_the_first_step() {
    let run = real_converged_run(GRAPH_WORKFLOW, &["writer", "fixer"], ALWAYS_CLEAN_AGENT);
    let pool = open(&run.db_path).await;
    let run_id = only_run_id(&pool).await;
    let rows = event_rows(&pool, &run_id).await;
    let kinds: Vec<&str> = rows.iter().map(|(kind, _)| kind.as_str()).collect();

    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == "workflow_resolved")
            .count(),
        1,
        "the resolved workflow graph must be recorded exactly once per run: {kinds:?}"
    );
    assert_eq!(
        &kinds[..2],
        &["run_started", "workflow_resolved"],
        "the graph must be recorded right after RunStarted, before the run does anything: {kinds:?}"
    );

    let payload = &rows[1].1;
    assert_eq!(payload["name"], "e2e-graph");
    assert_eq!(payload["entry"], 0);

    let stored_entry: i64 = sqlx::query("SELECT workflow_entry FROM runs WHERE id = ?")
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("workflow_entry");
    assert_eq!(
        payload["entry"].as_i64().unwrap(),
        stored_entry,
        "the recorded entry index must be the same index space the state machine runs on"
    );

    let steps = payload["steps"].as_array().unwrap();
    assert_eq!(
        steps.len(),
        3,
        "every declared step must be recorded: {steps:?}"
    );
    assert_eq!(
        steps
            .iter()
            .map(|step| step["index"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![0, 1, 2],
        "indices must be dense and follow Workflow::steps order"
    );

    assert_eq!(steps[0]["id"], "implementation");
    assert_eq!(steps[0]["kind"], "agent");
    assert_eq!(steps[0]["on_clean"], "verification");
    assert_eq!(
        steps[0]["on_blocking"], "implementation",
        "a self-loop is why a run reboucles -- it must be observable"
    );
    assert_eq!(steps[0]["on_error"], "failed");
    assert!(steps[0]["max_cycles"].is_null());
    assert_eq!(steps[0]["captures_evidence"], false);

    // Never executed by this run, yet fully described.
    assert_eq!(steps[1]["id"], "remediation");
    assert_eq!(steps[1]["kind"], "agent");
    assert_eq!(steps[1]["max_cycles"], 2);
    assert_eq!(steps[1]["captures_evidence"], true);
    assert!(
        !rows
            .iter()
            .any(|(kind, payload)| kind == "agent_started" && payload["role"] == "remediation"),
        "this scenario relies on `remediation` never executing: {rows:?}"
    );

    assert_eq!(steps[2]["id"], "verification");
    assert_eq!(
        steps[2]["kind"], "command",
        "agent and command steps must be told apart"
    );
    assert_eq!(steps[2]["on_clean"], "converged");
    assert_eq!(
        steps[2]["on_blocking"], "remediation",
        "a backward blocking edge must be observable"
    );
}

/// Acceptance criterion 1 under the condition the ticket exists for: a run that actually reboucles
/// (blocking finding -> backward transition -> extra cycles) still records the graph exactly once,
/// never once per cycle.
#[tokio::test]
async fn a_real_run_that_reboucles_still_records_the_graph_exactly_once() {
    let run = real_converged_run(
        REBOUCLE_WORKFLOW,
        &["writer", "reviewer"],
        BLOCKS_ONCE_AGENT,
    );
    let pool = open(&run.db_path).await;
    let run_id = only_run_id(&pool).await;
    let rows = event_rows(&pool, &run_id).await;
    let kinds: Vec<&str> = rows.iter().map(|(kind, _)| kind.as_str()).collect();

    assert!(
        kinds
            .iter()
            .filter(|kind| **kind == "cycle_started")
            .count()
            > 2,
        "this scenario must really reboucle, otherwise it proves nothing: {kinds:?}"
    );
    assert!(
        kinds.contains(&"finding_raised"),
        "the reboucle must be driven by a real blocking finding: {kinds:?}"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == "workflow_resolved")
            .count(),
        1,
        "a reboucling run must not re-record its graph per cycle: {kinds:?}"
    );
    assert_eq!(
        &kinds[..2],
        &["run_started", "workflow_resolved"],
        "{kinds:?}"
    );
}

/// A run that dies on its very first step is where a purely retrospective tree is least useful --
/// and where knowing the declared graph matters most. The graph must already be on record by then,
/// with the steps the run never got to.
#[tokio::test]
async fn a_real_run_that_fails_on_its_first_step_still_recorded_the_whole_graph() {
    let run = real_run(
        FAILING_WORKFLOW,
        &[],
        ALWAYS_CLEAN_AGENT,
        "finished: Failed",
    );
    let run_id = only_run_id(&open(&run.db_path).await).await;

    let model = replay_as_late_attach(&run.db_path, &run_id).await;
    let WorkflowGraph::Resolved(graph) = model.workflow_graph() else {
        panic!("a failed run must still expose its declared graph, got Unresolved");
    };
    assert_eq!(graph.name, "dies-early");
    assert_eq!(graph.steps.len(), 2);
    assert_eq!(
        declared(&graph, "publish").status,
        StepRuntimeStatus::NeverReached,
        "the step the run never got to must be reported as never reached: {graph:?}"
    );
    assert_eq!(declared(&graph, "publish").on_clean, "converged");
    assert_eq!(declared(&graph, "gate").on_error, "failed");
}

/// Acceptance criteria 2 and 3, through the real reader: attaching *after* the run has finished
/// still yields the full graph -- every declared step with its id, kind, three transitions, own
/// budget, and current execution status, including the step that was never reached.
#[tokio::test]
async fn a_late_attach_to_a_finished_real_run_knows_every_declared_step_including_unreached_ones() {
    let run = real_converged_run(GRAPH_WORKFLOW, &["writer", "fixer"], ALWAYS_CLEAN_AGENT);
    let run_id = only_run_id(&open(&run.db_path).await).await;

    let model = replay_as_late_attach(&run.db_path, &run_id).await;
    assert!(
        model.is_finished(),
        "the run must have ended before this late attach"
    );

    let WorkflowGraph::Resolved(graph) = model.workflow_graph() else {
        panic!("a late attach must learn the graph, got Unresolved");
    };
    assert_eq!(graph.name, "e2e-graph");
    assert_eq!(graph.entry, 0);
    assert_eq!(graph.steps.len(), 3);

    let implementation = declared(&graph, "implementation");
    assert_eq!(implementation.index, 0);
    assert_eq!(implementation.kind, "agent");
    assert_eq!(implementation.on_clean, "verification");
    assert_eq!(implementation.on_blocking, "implementation");
    assert_eq!(implementation.on_error, "failed");
    assert_eq!(implementation.max_cycles, None);
    assert_eq!(implementation.status, StepRuntimeStatus::Ran);

    let verification = declared(&graph, "verification");
    assert_eq!(verification.kind, "command");
    assert_eq!(verification.on_clean, "converged");
    assert_eq!(verification.on_blocking, "remediation");
    assert_eq!(verification.status, StepRuntimeStatus::Ran);

    // The whole point of the ticket: what was *never* done is still knowable.
    let remediation = declared(&graph, "remediation");
    assert_eq!(remediation.status, StepRuntimeStatus::NeverReached);
    assert_eq!(remediation.kind, "agent");
    assert_eq!(remediation.max_cycles, Some(2));
    assert!(remediation.captures_evidence);
    assert_eq!(remediation.on_clean, "verification");
    assert_eq!(remediation.on_blocking, "implementation");
    assert_eq!(remediation.on_error, "failed");
}

/// Acceptance criterion 4: a run predating issue #107 has no `workflow_resolved` row at all.
/// Deleting the row from a real database reproduces exactly that. The replay must stay usable and
/// silent -- no error, no panic, and every retrospective accessor still answering.
#[tokio::test]
async fn a_real_run_without_the_workflow_resolved_row_still_replays_retrospectively() {
    let run = real_converged_run(GRAPH_WORKFLOW, &["writer", "fixer"], ALWAYS_CLEAN_AGENT);
    let write_pool = open(&run.db_path).await;
    let run_id = only_run_id(&write_pool).await;

    let deleted =
        sqlx::query("DELETE FROM events WHERE run_id = ? AND event_type = 'workflow_resolved'")
            .bind(&run_id)
            .execute(&write_pool)
            .await
            .unwrap()
            .rows_affected();
    assert_eq!(deleted, 1, "the pre-#107 shape is one run with no such row");
    write_pool.close().await;

    let model = replay_as_late_attach(&run.db_path, &run_id).await;

    assert_eq!(
        model.workflow_graph(),
        WorkflowGraph::Unresolved,
        "a pre-#107 run must degrade to the retrospective view, not error"
    );
    assert!(
        model.undecodable_events().is_empty(),
        "nothing may be reported as undecodable: {:?}",
        model.undecodable_events()
    );
    assert!(model.is_finished());
    assert_eq!(model.final_state(), Some("converged"));
    assert!(
        model.run_started().is_some(),
        "the retrospective header must still render"
    );
    assert!(
        !model.workflow_tree().cycles.is_empty(),
        "the retrospective execution tree must still be derivable"
    );
}

/// The new event must survive the real writer -> real reader round trip *as a decoded event*: if
/// `EventKind::parse` or the payload tag ever drifts, `warden-tui` degrades it to an undecodable
/// row and the graph silently disappears -- which no assertion on a hand-built model would catch.
#[tokio::test]
async fn the_real_reader_decodes_the_recorded_graph_rather_than_tagging_it_undecodable() {
    let run = real_converged_run(GRAPH_WORKFLOW, &["writer", "fixer"], ALWAYS_CLEAN_AGENT);
    let run_id = only_run_id(&open(&run.db_path).await).await;

    let model = replay_as_late_attach(&run.db_path, &run_id).await;
    assert!(
        model.undecodable_events().is_empty(),
        "no row may fail to decode: {:?}",
        model.undecodable_events()
    );
    let resolved = model
        .events()
        .iter()
        .filter(|record| matches!(record.event, RunEvent::WorkflowResolved { .. }))
        .count();
    assert_eq!(resolved, 1, "{:?}", model.events());
}
