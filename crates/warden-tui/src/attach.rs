//! Attaching to a run: merges `events` history with the live Event Bus
//! stream so a late attach shows the full history and then seamlessly
//! switches to live, with no gap (Architecture.md §5.4, issue #8 acceptance
//! criterion 1).
//!
//! The order of operations is deliberate: **subscribe to the bus before
//! querying history**. Querying first and subscribing second would leave a
//! race window between the two calls during which a published event could
//! be missed entirely (too late for the history query to have seen it, too
//! early for the subscription to have started). Subscribing first means the
//! worst case is an event arriving on the live channel *before* the history
//! query returns -- handled by [`crate::model::RunModel::apply`]'s
//! id-based dedup once both sources are merged.

use std::path::Path;

use sqlx::SqlitePool;
use tokio::sync::mpsc;
use warden_core::RunEventRecord;

use crate::db;
use crate::error::{Result, TuiError};
use crate::model::RunModel;
use crate::subscriber;

/// The result of attaching to a run: a model already reflecting every event
/// known so far, plus (if the run is still live) a channel to keep applying
/// events from as they arrive.
pub struct Attachment {
    pub model: RunModel,
    /// `None` when the run has already finished (or was never live to begin
    /// with) and its Event Bus socket is gone -- a purely historical view,
    /// which is exactly as valid an attach target as a live one.
    pub live: Option<mpsc::UnboundedReceiver<RunEventRecord>>,
}

/// Attaches to `run_id`: verifies it exists, subscribes to its Event Bus
/// (best-effort -- a finished run's socket is expected to be gone), replays
/// its full `events` history, and folds in anything that arrived on the
/// live channel in the meantime.
pub async fn attach(pool: &SqlitePool, run_id: &str, socket_path: &Path) -> Result<Attachment> {
    if db::get_run(pool, run_id).await?.is_none() {
        return Err(TuiError::RunNotFound {
            run_id: run_id.to_string(),
        });
    }

    // Subscribe first (see module docs). A connection failure here just
    // means the run isn't live right now -- a finished run's socket file no
    // longer accepts connections, which is expected, not an error -- but it
    // must never be a *silent* one (code-standards.md: no catch-and-ignore).
    let mut live = match subscriber::subscribe(socket_path).await {
        Ok(rx) => Some(rx),
        Err(error) => {
            log_subscribe_failure(socket_path, &error);
            None
        }
    };

    let history = db::list_events_for_run(pool, run_id).await?;
    let mut model = RunModel::new();
    for record in history {
        model.apply(record);
    }

    if let Some(rx) = live.as_mut() {
        while let Ok(record) = rx.try_recv() {
            model.apply(record);
        }
    }

    Ok(Attachment { model, live })
}

/// Classifies why the initial subscribe attempt failed, and logs
/// accordingly, instead of silently discarding the error entirely
/// (code-standards.md: "catch-and-ignore (`.ok()` qui jette l'erreur sans
/// la logger)" is an explicitly named anti-pattern).
///
/// `NotFound` (the socket file was never created -- the run was never live,
/// or has already been cleaned up, see `warden::event_bus::EventBus`'s
/// `Drop` impl) and `ConnectionRefused` (the file still exists but nothing
/// is listening on it -- the orchestrator process that owned it has exited
/// without the file being removed yet) are both entirely ordinary ways to
/// discover "this run is not live right now", so they're logged at `debug`
/// only. Anything else (permission errors, and the like) is unexpected --
/// logged at `warn` so it doesn't disappear silently, while `attach` still
/// degrades to a history-only view rather than failing outright: a
/// `warden-tui` that can still read `events` is more useful than one that
/// refuses to attach at all over a live-socket-specific problem.
fn log_subscribe_failure(socket_path: &Path, error: &TuiError) {
    if is_expected_no_live_bus_error(error) {
        tracing::debug!(
            socket = %socket_path.display(),
            %error,
            "no live Event Bus for this run (expected for a finished or not-yet-started run)"
        );
    } else {
        tracing::warn!(
            socket = %socket_path.display(),
            %error,
            "failed to subscribe to the run's Event Bus for an unexpected reason; \
             falling back to a history-only attach"
        );
    }
}

/// `true` for the two ordinary ways a subscribe attempt fails when a run
/// simply isn't live right now -- see [`log_subscribe_failure`]'s docs.
/// Split out as its own pure predicate so the classification itself is
/// unit-testable without capturing log output.
fn is_expected_no_live_bus_error(error: &TuiError) -> bool {
    matches!(
        error,
        TuiError::Io(io_error)
            if matches!(
                io_error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;
    use warden_core::RunEvent;

    #[test]
    fn not_found_and_connection_refused_are_classified_as_expected() {
        assert!(is_expected_no_live_bus_error(&TuiError::Io(
            std::io::Error::from(std::io::ErrorKind::NotFound)
        )));
        assert!(is_expected_no_live_bus_error(&TuiError::Io(
            std::io::Error::from(std::io::ErrorKind::ConnectionRefused)
        )));
    }

    #[test]
    fn a_permission_error_is_not_classified_as_an_expected_no_live_bus_condition() {
        assert!(!is_expected_no_live_bus_error(&TuiError::Io(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied)
        )));
    }

    async fn seeded_pool(dir: &Path) -> SqlitePool {
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
        pool
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

    #[tokio::test]
    async fn attaching_to_an_unknown_run_is_a_typed_error() {
        let dir = TempDir::new().unwrap();
        let pool = seeded_pool(dir.path()).await;
        let socket_path = dir.path().join("no-run.sock");

        let result = attach(&pool, "does-not-exist", &socket_path).await;
        assert!(matches!(result, Err(TuiError::RunNotFound { .. })));
    }

    /// A run that has already finished (no listener on its socket) must
    /// still attach successfully, replaying its full history -- this is
    /// the normal way to inspect a past run, not an error condition.
    #[tokio::test]
    async fn attaching_to_a_finished_run_with_no_live_socket_replays_history_only() {
        let dir = TempDir::new().unwrap();
        let pool = seeded_pool(dir.path()).await;
        insert_event(
            &pool,
            "e1",
            &RunEvent::RunFinished {
                final_state: "converged".to_string(),
            },
            "2026-07-12T00:00:00+00:00",
        )
        .await;
        let socket_path = dir.path().join("gone.sock");

        let attachment = attach(&pool, "run-1", &socket_path).await.unwrap();
        assert!(attachment.live.is_none());
        assert!(attachment.model.is_finished());
        assert_eq!(attachment.model.final_state(), Some("converged"));
    }

    /// Acceptance criterion 1 (issue #8): a late attach on a running run
    /// must show its full history *and* switch to live with no gap.
    #[tokio::test]
    async fn attaching_to_a_live_run_replays_history_then_keeps_receiving_live_events() {
        let dir = TempDir::new().unwrap();
        let pool = seeded_pool(dir.path()).await;
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

        let socket_path = dir.path().join("run-1.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let live_record = RunEventRecord {
            id: "e3".to_string(),
            run_id: "run-1".to_string(),
            event: RunEvent::CycleStarted { cycle_number: 2 },
            created_at: "2026-07-12T00:00:02+00:00".to_string(),
        };
        let live_record_clone = live_record.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _addr) = listener.accept().await.unwrap();
            let line = serde_json::to_string(&live_record_clone).unwrap();
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.write_all(b"\n").await.unwrap();
            // Keep the connection open long enough for the client to read.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let mut attachment = attach(&pool, "run-1", &socket_path).await.unwrap();
        assert!(
            attachment.model.events().len() >= 2,
            "history (RunStarted, CycleStarted) must always have been replayed"
        );

        // The live event ("e3") may have already been folded in by
        // `attach()`'s own drain step if it arrived before the history
        // query returned, or may still be waiting on the channel -- both
        // are valid outcomes of "subscribe before querying history"; what
        // matters for issue #8's acceptance criterion ("no gap") is that it
        // is never lost either way.
        if attachment.model.events().len() < 3 {
            let live_rx = attachment.live.as_mut().expect("run is live");
            let received = tokio::time::timeout(std::time::Duration::from_secs(1), live_rx.recv())
                .await
                .expect("live event must arrive without the caller having to poll for it")
                .expect("live channel must not have closed");
            attachment.model.apply(received);
        }

        assert_eq!(
            attachment.model.events().len(),
            3,
            "no event may be lost across the replay/live boundary (issue #8 acceptance criterion 1)"
        );
        assert_eq!(attachment.model.current_cycle_number(), 2);

        server.await.unwrap();
    }
}
