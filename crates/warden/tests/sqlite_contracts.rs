use std::path::PathBuf;

use sqlx::SqlitePool;
use tempfile::TempDir;
use warden::db;
use warden_core::{RunEvent, RunState};

const RUN_ID: &str = "sqlite-contract-run";

async fn migrated_writer_db() -> (TempDir, PathBuf, SqlitePool) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let pool = db::connect(&db_path).await.unwrap();
    let repo_path = dir.path().display().to_string();
    db::insert_run(
        &pool,
        RUN_ID,
        &repo_path,
        "main",
        "verify SQLite readers",
        4,
        3,
        3,
        5,
    )
    .await
    .unwrap();
    (dir, db_path, pool)
}

#[tokio::test]
async fn gated_reads_a_run_written_through_the_migrated_writer_schema() {
    let (_dir, db_path, write_pool) = migrated_writer_db().await;
    db::update_run_state(&write_pool, RUN_ID, RunState::AwaitingCi)
        .await
        .unwrap();
    db::set_run_converged_commit(&write_pool, RUN_ID, "deadbeef")
        .await
        .unwrap();
    db::set_run_pr_number(&write_pool, RUN_ID, 42)
        .await
        .unwrap();
    write_pool.close().await;

    let read_pool = warden_gated::db::connect_read_only(&db_path).await.unwrap();
    let gate_view = warden_gated::db::get_run_view(&read_pool, RUN_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gate_view.state, RunState::AwaitingCi);
    assert_eq!(gate_view.converged_commit_sha.as_deref(), Some("deadbeef"));

    let ci_view = warden_gated::db::get_awaiting_ci_run_view(&read_pool, RUN_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ci_view.state, RunState::AwaitingCi);
    assert_eq!(ci_view.pr_number, Some(42));
}

#[tokio::test]
async fn tui_reads_a_run_and_event_written_through_the_migrated_writer_schema() {
    let (_dir, db_path, write_pool) = migrated_writer_db().await;
    db::update_run_state(&write_pool, RUN_ID, RunState::CoderRunning)
        .await
        .unwrap();
    let event = RunEvent::RunStarted {
        intent: "verify SQLite readers".to_string(),
        branch: "main".to_string(),
        max_review_cycles: 4,
        max_test_cycles: 3,
    };
    db::insert_event(
        &write_pool,
        "event-1",
        RUN_ID,
        &event,
        "2026-08-02T12:00:00Z",
    )
    .await
    .unwrap();
    write_pool.close().await;

    let read_pool = warden_tui::db::connect_read_only(&db_path).await.unwrap();
    let run = warden_tui::db::get_run(&read_pool, RUN_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.intent, "verify SQLite readers");
    assert_eq!(run.branch, "main");
    assert_eq!(run.state, RunState::CoderRunning);
    assert_eq!(run.max_review_cycles, 4);
    assert_eq!(run.max_test_cycles, 3);

    let events = warden_tui::db::list_events_for_run(&read_pool, RUN_ID)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event(), Some(&event));
}
