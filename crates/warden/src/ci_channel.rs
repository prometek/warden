//! The reverse CI-result channel (issue #15/ADR-0011): `warden` binds a Unix
//! socket per run at `resolve_ci_result_socket_path`, hardened to `0600`,
//! mirroring `warden_gated::relay`'s forward socket and
//! `event_bus::EventBus`'s identical hardening. `warden-gated` connects to
//! it once a run's post-`Converged` tail (push + PR open/finalize + CI
//! watch) reaches a terminal outcome, and delivers exactly one
//! [`CiResultMessage`] as a raw byte payload -- parsed at this boundary,
//! never trusted, mirroring `warden_gated::notification::parse_post_receive_line`'s
//! discipline: a malformed delivery is a typed error, never silently
//! ignored.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use warden_core::{parse_ci_result_message, resolve_ci_result_socket_path, CiResultMessage};

use crate::error::Result;

/// A per-run reverse-channel listener, bound for the lifetime of that run's
/// wait on `AwaitingCi` (opened when entering it, including during
/// crash-recovery reconciliation -- see `orchestrator::recover_crashed_runs`).
pub struct CiResultListener {
    socket_path: PathBuf,
    listener: UnixListener,
}

impl CiResultListener {
    /// Binds a fresh listener at `resolve_ci_result_socket_path(run_id,
    /// runs_dir)`, removing a stale socket file left over from a previous
    /// attempt at the same path first (a Unix socket path can't be re-bound
    /// while the old inode still exists) -- this is what makes re-binding
    /// idempotent across a crash-recovery re-request for the same run.
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

    /// Accepts exactly one connection, reads its payload to EOF, and parses
    /// it into a [`CiResultMessage`] (ADR-0011: "un seul message terminal
    /// par run", not a stream). No timeout of its own: `watch_pr`'s own
    /// inactivity timeout bounds how long `warden-gated` can take before it
    /// sends *something*, and GitHub is the durable source of truth if this
    /// process is restarted before delivery completes -- crash recovery
    /// re-requests the watch rather than this call ever needing to give up
    /// on its own (ADR-0011 "Conséquences").
    pub async fn receive(&self) -> Result<CiResultMessage> {
        let (mut stream, _addr) = self.listener.accept().await?;
        let mut buffer = String::new();
        stream.read_to_string(&mut buffer).await?;
        Ok(parse_ci_result_message(&buffer)?)
    }
}

impl Drop for CiResultListener {
    /// Best-effort removal of the socket file once this listener goes out of
    /// scope -- mirrors `event_bus::EventBus`'s identical cleanup. A failure
    /// here (already gone, a benign race) is not worth surfacing: whatever
    /// this listener was waiting for has already been resolved one way or
    /// another by the time it's dropped.
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

/// Restricts `socket_path` to owner-only read/write (`0600`), matching the
/// permission `event_bus::EventBus::bind`/`warden_gated::relay::bind` both
/// already apply to their own sockets.
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

        let (_sent, received) = tokio::join!(sender, listener.receive());
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

        let (_sent, received) = tokio::join!(sender, listener.receive());
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
}
