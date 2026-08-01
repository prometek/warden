use std::path::Path;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use warden_core::RunEventRecord;

use crate::error::Result;

pub use warden_core::resolve_socket_path;

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
