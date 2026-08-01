//! Unix domain socket transport between the `post-receive` hook (via the `notify` relay subcommand)
//! and the long-running `serve` daemon.

use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::error::Result;

/// Relays `payload` (the hook's raw stdin) to the daemon listening on `socket_path`.
pub async fn relay(socket_path: &Path, payload: &[u8]) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path).await?;
    stream.write_all(payload).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Binds a fresh listener at `socket_path`, removing a stale socket file left over from a previous
/// run first -- a Unix socket path can't be re-bound while the old inode still exists.
pub async fn bind(socket_path: &Path) -> Result<UnixListener> {
    if socket_path.exists() {
        tokio::fs::remove_file(socket_path).await?;
    }

    let listener = UnixListener::bind(socket_path)?;

    #[cfg(unix)]
    harden_socket_permissions(socket_path).await?;

    Ok(listener)
}

/// Restricts `socket_path` to owner-only read/write (`0600`).
#[cfg(unix)]
async fn harden_socket_permissions(socket_path: &Path) -> Result<()> {
    let mut permissions = tokio::fs::metadata(socket_path).await?.permissions();
    permissions.set_mode(0o600);
    tokio::fs::set_permissions(socket_path, permissions).await?;
    Ok(())
}

/// Reads a relayed payload to completion (the hook side always shuts its write half down after
/// sending, see [`relay`], so EOF marks the end of one notification).
pub async fn read_payload(stream: &mut UnixStream) -> Result<String> {
    let mut buffer = String::new();
    stream.read_to_string(&mut buffer).await?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn relay_delivers_the_exact_bytes_sent() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("gated.sock");
        let listener = bind(&socket_path).await.unwrap();

        let payload = b"old111 new222 refs/heads/warden-run/run-abc\n";
        let send = relay(&socket_path, payload);
        let accept = async {
            let (mut stream, _addr) = listener.accept().await.unwrap();
            read_payload(&mut stream).await.unwrap()
        };

        let (send_result, received) = tokio::join!(send, accept);
        send_result.unwrap();
        assert_eq!(received, String::from_utf8_lossy(payload));
    }

    #[tokio::test]
    async fn bind_replaces_a_stale_socket_file_left_by_a_previous_run() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("gated.sock");

        std::fs::write(&socket_path, b"not a real socket").unwrap();

        let listener = bind(&socket_path).await;
        assert!(listener.is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_restricts_the_socket_file_to_owner_only_read_write() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("gated.sock");
        let _listener = bind(&socket_path).await.unwrap();

        let mode = std::fs::metadata(&socket_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "socket must be owner-only read/write, got mode {mode:o}"
        );
    }
}
