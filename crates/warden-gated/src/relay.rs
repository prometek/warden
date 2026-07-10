//! Unix domain socket transport between the `post-receive` hook (via the
//! `notify` relay subcommand) and the long-running `serve` daemon.
//! Strictly a byte pipe in both directions -- whatever arrives on the
//! hook's stdin is forwarded verbatim, and it's the daemon side
//! (`serve::handle_payload`) that parses and decides, never this module
//! (code-standards.md: "aucune logique métier dans les ... callbacks
//! d'event").

use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
///
/// This socket is an unauthenticated local trigger into the sole holder of
/// `origin`'s credentials: anyone who can connect to it can make the daemon
/// attempt a push (the actual push decision is still independently
/// re-verified against SQLite, but there's no reason to let another local
/// user even attempt it). Hardened to `0600` right after bind, matching the
/// `0600` permission ADR-0008 already mandates for the analogous
/// `warden-tui` event bus socket -- `bind(2)` creates the socket file with
/// the umask-derived default mode, which is not narrow enough on its own.
pub async fn bind(socket_path: &Path) -> Result<UnixListener> {
    if socket_path.exists() {
        tokio::fs::remove_file(socket_path).await?;
    }

    let listener = UnixListener::bind(socket_path)?;

    #[cfg(unix)]
    harden_socket_permissions(socket_path).await?;

    Ok(listener)
}

/// Restricts `socket_path` to owner-only read/write (`0600`). The
/// containing directory's permissions are the deployment's responsibility
/// (e.g. `~/.warden` is expected to already be private to its owner) --
/// this module only narrows the one file it creates.
#[cfg(unix)]
async fn harden_socket_permissions(socket_path: &Path) -> Result<()> {
    let mut permissions = tokio::fs::metadata(socket_path).await?.permissions();
    permissions.set_mode(0o600);
    tokio::fs::set_permissions(socket_path, permissions).await?;
    Ok(())
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

    /// MEDIUM finding (issue #3 review): the socket is an unauthenticated
    /// local trigger into the sole credential holder -- must be `0600` so
    /// only its owner can even attempt to connect, matching ADR-0008's
    /// `0600` for the analogous TUI socket.
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
