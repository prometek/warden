use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use warden_core::{parse_ci_result_message, resolve_ci_result_socket_path, CiResultMessage};

use crate::error::{Result, WardenError};

const MAX_CI_RESULT_PAYLOAD_BYTES: usize = 1024 * 1024;

pub struct CiResultListener {
    socket_path: PathBuf,
    listener: UnixListener,
}

impl CiResultListener {
    pub async fn bind(run_id: &str, runs_dir: &Path) -> Result<Self> {
        let socket_path = resolve_ci_result_socket_path(run_id, runs_dir);
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if socket_path.exists() {
            tokio::fs::remove_file(&socket_path).await?;
        }

        let listener = UnixListener::bind(&socket_path)?;

        #[cfg(unix)]
        harden_socket_permissions(&socket_path).await?;

        Ok(Self {
            socket_path,
            listener,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Accepts exactly one connection, reads its payload to EOF, and parses it into a
    /// [`CiResultMessage`], bounded by a wall-clock `timeout`.
    pub async fn receive(&self, timeout: Duration) -> Result<CiResultMessage> {
        let run_or_timeout = tokio::time::timeout(timeout, self.receive_unbounded()).await;
        match run_or_timeout {
            Ok(result) => result,
            Err(_elapsed) => Err(WardenError::CiResultTimedOut {
                run_id: run_id_from_socket_path(&self.socket_path),
                timeout_secs: timeout.as_secs(),
            }),
        }
    }

    /// Accepts one connection and parses one [`CiResultMessage`] with no timeout of its own.
    pub async fn receive_no_timeout(&self) -> Result<CiResultMessage> {
        self.receive_unbounded().await
    }

    async fn receive_unbounded(&self) -> Result<CiResultMessage> {
        let (mut stream, _addr) = self.listener.accept().await?;
        let mut limited = (&mut stream).take(MAX_CI_RESULT_PAYLOAD_BYTES as u64 + 1);
        let mut buffer = String::new();
        limited.read_to_string(&mut buffer).await?;
        if buffer.len() > MAX_CI_RESULT_PAYLOAD_BYTES {
            return Err(WardenError::CiResultPayloadTooLarge {
                max_bytes: MAX_CI_RESULT_PAYLOAD_BYTES,
            });
        }
        Ok(parse_ci_result_message(&buffer)?)
    }
}

fn run_id_from_socket_path(socket_path: &Path) -> String {
    socket_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.strip_suffix(".ci"))
        .map(str::to_string)
        .unwrap_or_else(|| socket_path.display().to_string())
}

impl Drop for CiResultListener {
    /// Best-effort removal of the socket file once this listener goes out of scope -- mirrors
    /// `event_bus::EventBus`'s identical cleanup.
    fn drop(&mut self) {
        match std::fs::remove_file(&self.socket_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::debug!(
                    socket = %self.socket_path.display(),
                    %error,
                    "failed to remove CI result socket file on shutdown"
                );
            }
        }
    }
}

/// Restricts `socket_path` to owner-only read/write (`0600`), matching the permission
/// `event_bus::EventBus::bind`/`warden_gated::relay::bind` both already apply to their own sockets.
#[cfg(unix)]
async fn harden_socket_permissions(socket_path: &Path) -> Result<()> {
    let mut permissions = tokio::fs::metadata(socket_path).await?.permissions();
    permissions.set_mode(0o600);
    tokio::fs::set_permissions(socket_path, permissions).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;
    use warden_core::CiWatchOutcome;

    fn sample_message() -> CiResultMessage {
        CiResultMessage {
            run_id: "run-1".to_string(),
            pr_number: Some(7),
            outcome: CiWatchOutcome::checks_passed(),
        }
    }

    #[tokio::test]
    async fn bind_creates_a_socket_restricted_to_owner_only_read_write() {
        let dir = TempDir::new().unwrap();
        let listener = CiResultListener::bind("run-1", dir.path()).await.unwrap();

        #[cfg(unix)]
        {
            let mode = std::fs::metadata(listener.socket_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "CI result socket must be owner-only");
        }
    }

    #[tokio::test]
    async fn receive_parses_a_delivered_message() {
        let dir = TempDir::new().unwrap();
        let listener = CiResultListener::bind("run-1", dir.path()).await.unwrap();
        let socket_path = listener.socket_path().to_path_buf();
        let message = sample_message();

        let sender = {
            let message = message.clone();
            async move {
                let mut stream = UnixStream::connect(&socket_path).await.unwrap();
                stream
                    .write_all(message.to_json().unwrap().as_bytes())
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
            }
        };

        let (_sent, received) = tokio::join!(sender, listener.receive(Duration::from_secs(5)));
        assert_eq!(received.unwrap(), message);
    }

    #[tokio::test]
    async fn receive_rejects_a_malformed_delivery_as_a_typed_error() {
        let dir = TempDir::new().unwrap();
        let listener = CiResultListener::bind("run-1", dir.path()).await.unwrap();
        let socket_path = listener.socket_path().to_path_buf();

        let sender = async move {
            let mut stream = UnixStream::connect(&socket_path).await.unwrap();
            stream.write_all(b"not json").await.unwrap();
            stream.shutdown().await.unwrap();
        };

        let (_sent, received) = tokio::join!(sender, listener.receive(Duration::from_secs(5)));
        assert!(received.is_err());
    }

    #[tokio::test]
    async fn dropping_the_listener_removes_its_socket_file() {
        let dir = TempDir::new().unwrap();
        let listener = CiResultListener::bind("run-1", dir.path()).await.unwrap();
        let socket_path = listener.socket_path().to_path_buf();
        assert!(socket_path.exists());

        drop(listener);

        assert!(!socket_path.exists());
    }

    #[tokio::test]
    async fn bind_replaces_a_stale_socket_file_left_by_a_previous_attempt() {
        let dir = TempDir::new().unwrap();
        let socket_path = warden_core::resolve_ci_result_socket_path("run-1", dir.path());
        std::fs::write(&socket_path, b"not a real socket").unwrap();

        let listener = CiResultListener::bind("run-1", dir.path()).await;
        assert!(listener.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn receive_times_out_when_nothing_is_ever_delivered() {
        let dir = TempDir::new().unwrap();
        let listener = CiResultListener::bind("run-1", dir.path()).await.unwrap();

        let result = listener.receive(Duration::from_secs(10)).await;

        assert!(matches!(result, Err(WardenError::CiResultTimedOut { .. })));
    }

    #[tokio::test]
    async fn receive_rejects_a_payload_past_the_size_cap() {
        let dir = TempDir::new().unwrap();
        let listener = CiResultListener::bind("run-1", dir.path()).await.unwrap();
        let socket_path = listener.socket_path().to_path_buf();

        let sender = async move {
            let mut stream = UnixStream::connect(&socket_path).await.unwrap();
            let oversized = vec![b'a'; MAX_CI_RESULT_PAYLOAD_BYTES + 1024];
            stream.write_all(&oversized).await.unwrap();
            stream.shutdown().await.unwrap();
        };

        let (_sent, received) = tokio::join!(sender, listener.receive(Duration::from_secs(5)));

        assert!(matches!(
            received,
            Err(WardenError::CiResultPayloadTooLarge { .. })
        ));
    }
}
