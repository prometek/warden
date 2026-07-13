//! Subscribes to a run's Event Bus (ADR-0008): connects to the Unix socket
//! `warden::event_bus::EventBus` publishes on and forwards every decoded
//! [`RunEventRecord`] onto an unbounded local channel for [`crate::attach`]
//! to merge with history.
//!
//! Strictly read-only by construction: this module only ever calls `read`
//! on the connection, never `write` -- there is no code path here through
//! which `warden-tui` could send anything back to the orchestrator
//! (code-standards.md: "la TUI n'émet jamais vers Warden").

use std::path::Path;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use warden_core::RunEventRecord;

use crate::error::Result;

/// Re-exported rather than duplicated: unlike `db.rs` (deliberately
/// duplicated from `warden::db`, ADR-0006, to keep two crates' *query
/// correctness* independently re-verified), the socket path is a single
/// wire-addressing rule both the publisher (`warden::event_bus`) and this
/// subscriber must agree on byte-for-byte -- see `warden_core::socket`'s
/// module docs for why it lives there instead. Re-exported (not just
/// called directly at each use site) so existing callers/tests referencing
/// `subscriber::resolve_socket_path` keep working unchanged.
pub use warden_core::resolve_socket_path;

/// Connects to `socket_path` and spawns a background task that decodes each
/// NDJSON line into a [`RunEventRecord`] and forwards it on the returned
/// channel, until the connection closes (the run's `EventBus` was dropped,
/// or the orchestrator process exited) or a malformed line is received.
///
/// A malformed line is logged and ends the subscription rather than being
/// skipped: the wire protocol is `warden`'s own `EventBus`, not untrusted
/// agent output, so a decode failure here means something is genuinely
/// wrong (protocol drift, a corrupted stream) worth surfacing loudly rather
/// than silently dropping events (code-standards.md: "no silent fallback").
pub async fn subscribe(socket_path: &Path) -> Result<mpsc::UnboundedReceiver<RunEventRecord>> {
    let stream = UnixStream::connect(socket_path).await?;
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match serde_json::from_str::<RunEventRecord>(&line) {
                    Ok(record) => {
                        if tx.send(record).is_err() {
                            // Receiver dropped (the TUI is shutting down) --
                            // nothing left to forward to.
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, line, "event bus subscriber: malformed event line");
                        break;
                    }
                },
                Ok(None) => break, // Clean EOF: the bus/orchestrator went away.
                Err(error) => {
                    tracing::error!(%error, "event bus subscriber: read error");
                    break;
                }
            }
        }
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;
    use warden_core::RunEvent;

    #[tokio::test]
    async fn subscribe_decodes_events_published_on_the_socket() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("run-1.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let record = RunEventRecord {
            id: "event-1".to_string(),
            run_id: "run-1".to_string(),
            event: RunEvent::CycleStarted { cycle_number: 1 },
            created_at: "2026-07-12T00:00:00+00:00".to_string(),
        };
        let record_clone = record.clone();

        let server = tokio::spawn(async move {
            let (mut stream, _addr) = listener.accept().await.unwrap();
            let line = serde_json::to_string(&record_clone).unwrap();
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.write_all(b"\n").await.unwrap();
        });

        let mut rx = subscribe(&socket_path).await.unwrap();
        let received = rx.recv().await.unwrap();
        assert_eq!(received, record);

        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_fails_when_nothing_is_listening() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("no-such-run.sock");

        let result = subscribe(&socket_path).await;
        assert!(result.is_err());
    }

    // `resolve_socket_path` itself is tested in `warden_core::socket`, the
    // single shared implementation this re-export delegates to (see this
    // module's docs) -- this is just a smoke test that the re-export is
    // actually wired to it.
    #[test]
    fn resolve_socket_path_re_export_delegates_to_warden_core() {
        let runs_dir = Path::new("/tmp/warden/runs");
        let run_id = "11111111-1111-1111-1111-111111111111";
        assert_eq!(
            resolve_socket_path(run_id, runs_dir),
            warden_core::resolve_socket_path(run_id, runs_dir)
        );
    }
}
