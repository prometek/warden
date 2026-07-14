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
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use warden_core::{parse_ci_result_message, resolve_ci_result_socket_path, CiResultMessage};

use crate::error::{Result, WardenError};

/// Issue #15 review, L1: caps how many bytes [`CiResultListener::receive`]
/// will buffer from one connection before refusing it outright, rather than
/// reading an unbounded stream to completion (`read_to_string` has no size
/// limit of its own). A real `CiResultMessage` -- JSON, a handful of fields,
/// at most a few dozen `ChecksFailed` findings -- is nowhere near this; a
/// sender that exceeds it is either malfunctioning or hostile, not a
/// legitimate delivery that just happens to be large.
const MAX_CI_RESULT_PAYLOAD_BYTES: usize = 1024 * 1024;

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

    /// Accepts exactly one connection, reads its payload to EOF (capped at
    /// [`MAX_CI_RESULT_PAYLOAD_BYTES`], issue #15 review, L1), and parses it
    /// into a [`CiResultMessage`] (ADR-0011: "un seul message terminal par
    /// run", not a stream).
    ///
    /// Bounded by `timeout` (issue #15 review, H1(b)): the accept-and-read
    /// as a whole must complete within it, or this returns
    /// [`WardenError::CiResultTimedOut`] rather than awaiting forever.
    /// `watch_pr`'s own inactivity timeout bounds how long `warden-gated`
    /// can take before it sends *something* in the common case, but this
    /// call cannot simply trust that on its own -- `warden-gated` could be
    /// dead, unreachable, or stuck before it even starts watching. GitHub
    /// remains the durable source of truth if the run is later retried;
    /// this timeout only stops `warden` itself from blocking indefinitely on
    /// one run.
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

    /// Accepts one connection and parses one [`CiResultMessage`] with no
    /// timeout of its own (issue #15 review, M-new-1). The orchestrator drives
    /// this inside a `select!` against the triggered subprocess's liveness
    /// ([`crate::gate_trigger::GateChild`]), so the wait is bounded by the
    /// child being alive rather than a wall-clock guess that can't match
    /// `watch_pr`'s uncapped runtime. Same size cap as [`Self::receive`].
    pub async fn receive_no_timeout(&self) -> Result<CiResultMessage> {
        self.receive_unbounded().await
    }

    async fn receive_unbounded(&self) -> Result<CiResultMessage> {
        let (mut stream, _addr) = self.listener.accept().await?;
        // `.take(N)` caps how much `read_to_string` will ever buffer; +1
        // lets an exactly-`N`-byte legitimate payload be told apart from one
        // that keeps going past the cap (which would otherwise both read
        // back as exactly `N` bytes with no way to distinguish them).
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

/// Best-effort extraction of the run id this listener's socket file name
/// encodes, purely for [`WardenError::CiResultTimedOut`]'s error message --
/// `resolve_ci_result_socket_path`'s own fallback-to-temp-dir naming means
/// this isn't always recoverable from the path alone, so a failure here
/// falls back to the literal path rather than erroring.
fn run_id_from_socket_path(socket_path: &Path) -> String {
    socket_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.strip_suffix(".ci"))
        .map(str::to_string)
        .unwrap_or_else(|| socket_path.display().to_string())
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

    /// Issue #15 review, H1(b): `receive` must never await forever -- with
    /// nothing ever connecting, it must give up after `timeout` with a
    /// typed error, not hang the test (or, in production, `warden` itself).
    #[tokio::test(start_paused = true)]
    async fn receive_times_out_when_nothing_is_ever_delivered() {
        let dir = TempDir::new().unwrap();
        let listener = CiResultListener::bind("run-1", dir.path()).await.unwrap();

        let result = listener.receive(Duration::from_secs(10)).await;

        assert!(matches!(result, Err(WardenError::CiResultTimedOut { .. })));
    }

    /// Issue #15 review, L1: a payload past the size cap must be refused,
    /// not buffered without bound.
    #[tokio::test]
    async fn receive_rejects_a_payload_past_the_size_cap() {
        let dir = TempDir::new().unwrap();
        let listener = CiResultListener::bind("run-1", dir.path()).await.unwrap();
        let socket_path = listener.socket_path().to_path_buf();

        let sender = async move {
            let mut stream = UnixStream::connect(&socket_path).await.unwrap();
            // Comfortably past MAX_CI_RESULT_PAYLOAD_BYTES; content doesn't
            // matter, only its size does.
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
