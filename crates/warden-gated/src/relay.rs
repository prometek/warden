//! Unix domain socket transport between the `post-receive` hook (via the
//! `notify` relay subcommand) and the long-running `serve` daemon.
//! Strictly a byte pipe in both directions -- whatever arrives on the
//! hook's stdin is forwarded verbatim, and it's the daemon side
//! (`serve::handle_payload`) that parses and decides, never this module
//! (code-standards.md: "aucune logique métier dans les ... callbacks
//! d'event").

use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::error::Result;

/// Relays `payload` (the hook's raw stdin) to the daemon listening on
/// `socket_path`. No parsing, no retry -- a connection failure surfaces
/// directly to the hook's exit code rather than being swallowed, so a
/// misconfigured/dead daemon is visible as a failed `git push` instead of a
/// silently dropped notification.
pub async fn relay(socket_path: &Path, payload: &[u8]) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path).await?;
    stream.write_all(payload).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Binds a fresh listener at `socket_path`, removing a stale socket file
/// left over from a previous run first -- a Unix socket path can't be
/// re-bound while the old inode still exists.
pub async fn bind(socket_path: &Path) -> Result<UnixListener> {
    if socket_path.exists() {
        tokio::fs::remove_file(socket_path).await?;
    }
    Ok(UnixListener::bind(socket_path)?)
}

/// Reads a relayed payload to completion (the hook side always shuts its
/// write half down after sending, see [`relay`], so EOF marks the end of
/// one notification).
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

        // Simulate a leftover socket file from a daemon that didn't clean up.
        std::fs::write(&socket_path, b"not a real socket").unwrap();

        let listener = bind(&socket_path).await;
        assert!(listener.is_ok());
    }
}
