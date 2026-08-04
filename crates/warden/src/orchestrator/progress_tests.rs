//! Issue #108 at the publication seam: `AgentProgress` is now persisted, and that must change
//! *nothing* about how it is published.
//!
//! The volume policy itself (per-step cap, saturation drops, batching) is pinned in
//! `crate::progress_writer`; what is pinned here is the coupling between the two paths -- or rather
//! the absence of one.

use std::path::PathBuf;

use super::*;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use warden_core::ProgressReplay;

const RUN_ID: &str = "progress-run";

async fn seeded_pool(dir: &TempDir) -> SqlitePool {
    let pool = db::connect(&dir.path().join("state.db")).await.unwrap();
    db::insert_run(&pool, RUN_ID, "/tmp/repo", "main", "intent", 3, 3, 1, 3)
        .await
        .unwrap();
    pool
}

/// An orchestrator with a bound run context -- the state `run_agent` would normally have set up
/// before a single line of agent stdout could arrive.
async fn orchestrator_with_run_context(
    dir: &TempDir,
    pool: SqlitePool,
    progress_writer: ProgressWriter,
) -> (Orchestrator, PathBuf) {
    let event_bus = EventBus::bind(RUN_ID, &dir.path().join("runs"))
        .await
        .unwrap();
    let socket_path = event_bus.socket_path().to_path_buf();

    let orchestrator = Orchestrator::new(pool);
    orchestrator
        .run_context
        .set(RunContext {
            run_id: RUN_ID.to_string(),
            event_bus,
            progress_writer,
        })
        .map_err(|_| ())
        .unwrap();
    (orchestrator, socket_path)
}

async fn persisted_progress(pool: &SqlitePool, progress: ProgressReplay) -> Vec<String> {
    db::list_events_for_run(pool, RUN_ID, progress)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry.event() {
            Some(RunEvent::AgentProgress { detail, .. }) => Some(detail.clone()),
            _ => None,
        })
        .collect()
}

/// The live path is unchanged by persistence: a subscriber sees every progress event even when the
/// writer behind it drops all of them -- a saturated queue, a dead writer task, a cap reached.
#[tokio::test]
async fn a_progress_event_the_writer_drops_is_still_delivered_live() {
    let dir = TempDir::new().unwrap();
    let pool = seeded_pool(&dir).await;
    let (orchestrator, socket_path) =
        orchestrator_with_run_context(&dir, pool.clone(), ProgressWriter::disconnected()).await;
    let mut subscriber = UnixStream::connect(&socket_path).await.unwrap();
    // The bus spawns one forwarding task per accepted connection; publishing before that task has
    // subscribed to the broadcast channel would race it.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    for index in 0..3 {
        orchestrator.publish_progress_event("implementation", format!("line-{index}"));
    }

    let mut reader = BufReader::new(&mut subscriber);
    let mut received = Vec::new();
    for _ in 0..3 {
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await
        .expect("a dropped write must never withhold the live event")
        .unwrap();
        let record: warden_core::RunEventRecord = serde_json::from_str(line.trim()).unwrap();
        match record.event {
            RunEvent::AgentProgress { detail, .. } => received.push(detail),
            other => panic!("expected progress, got {other:?}"),
        }
    }

    assert_eq!(received, vec!["line-0", "line-1", "line-2"]);
    orchestrator.flush_progress().await;
    assert!(
        persisted_progress(&pool, ProgressReplay::Included)
            .await
            .is_empty(),
        "precondition of this test: none of the three was persisted"
    );
}

/// Acceptance criterion: what a replay exposes is what a live subscriber saw, in the order it was
/// published -- and the run's own state is untouched by any of it.
#[tokio::test]
async fn published_progress_is_persisted_in_publication_order_and_hidden_from_a_default_replay() {
    let dir = TempDir::new().unwrap();
    let pool = seeded_pool(&dir).await;
    let (orchestrator, _socket_path) =
        orchestrator_with_run_context(&dir, pool.clone(), ProgressWriter::spawn(pool.clone()))
            .await;

    orchestrator.begin_progress_step();
    for index in 0..50 {
        orchestrator.publish_progress_event("implementation", format!("line-{index}"));
    }
    orchestrator.flush_progress().await;

    let expected: Vec<String> = (0..50).map(|index| format!("line-{index}")).collect();
    assert_eq!(
        persisted_progress(&pool, ProgressReplay::Included).await,
        expected,
        "an opted-in replay must yield the live sequence, in publication order"
    );
    assert!(
        persisted_progress(&pool, ProgressReplay::Excluded)
            .await
            .is_empty(),
        "a default replay must not carry progress at all"
    );
    assert_eq!(
        db::get_run(&pool, RUN_ID).await.unwrap().unwrap().state,
        RunState::Pending,
        "persisting progress must never touch the run's own state"
    );
}
