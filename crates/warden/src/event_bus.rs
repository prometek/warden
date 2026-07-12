//! The Event Bus (ADR-0008, issue #8): a local Unix socket at
//! `<warden_home>/runs/<run_id>.sock`, permissions `0600`, that broadcasts
//! every [`RunEvent`] the orchestrator publishes to every currently
//! connected `warden-tui` subscriber.
//!
//! Strictly read-only/unidirectional by construction, not just by
//! convention: [`accept_loop`]/[`forward_to_subscriber`] never call
//! `read`/`recv` on an accepted connection, only `write` -- there is no code
//! path in this module through which bytes arriving from a subscriber could
//! ever reach the orchestrator (code-standards.md, "Inter-process
//! Communication": "Unidirectionnel — la TUI n'émet jamais vers Warden").
//!
//! A slow or disconnected subscriber must never block or fail the
//! publisher (ADR-0008: "Un abonné qui se déconnecte ne doit jamais bloquer
//! l'orchestrateur"): [`EventBus::publish`] broadcasts over a bounded
//! `tokio::sync::broadcast` channel, whose defined behaviour is to drop the
//! oldest buffered events for a subscriber that falls behind rather than
//! apply backpressure to the sender -- exactly the "canal non bloquant /
//! bornée avec perte tolérée côté publication" ADR-0008 asks for.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use warden_core::RunEventRecord;

use crate::error::Result;

/// Bound on how many not-yet-delivered events a single slow subscriber can
/// lag behind before older ones are dropped for it (see module docs). Named
/// rather than left at whatever `broadcast::channel`'s caller happens to
/// pick, since it directly trades off "how much history survives a
/// subscriber hiccup" against "how much memory a stalled subscriber can pin".
const CHANNEL_CAPACITY: usize = 256;

/// Conservative usable length for `sockaddr_un.sun_path`. The real limit is
/// platform-specific (104 bytes total including the NUL terminator on
/// macOS/BSD, 108 on Linux) -- 100 leaves headroom on the tighter of the
/// two rather than cutting it exactly at the boundary.
const MAX_SOCKET_PATH_LEN: usize = 100;

/// Resolves where a run's Event Bus socket should live: the ADR-0008-
/// mandated `<runs_dir>/<run_id>.sock` when that fits within
/// [`MAX_SOCKET_PATH_LEN`], otherwise a short, deterministic path under the
/// OS temp directory keyed by `run_id` alone.
///
/// `runs_dir` comes from `--warden-home` (user-controlled, and `~/.warden`
/// itself may already sit under a deep sandboxed/containerized home
/// directory) concatenated with a UUID run id -- comfortably within limits
/// for a typical `$HOME`, but not guaranteed for every deployment. Binding
/// would otherwise fail outright with an opaque `EINVAL`/`SUN_LEN` OS error;
/// falling back to a short, still run-id-keyed path keeps the Event Bus
/// working rather than silently disabling Phase 8 observability for anyone
/// whose `--warden-home` happens to resolve to a long path. `warden-tui`
/// must derive the exact same path the same way to find it -- see the
/// mirrored copy of this function in that crate.
fn resolve_socket_path(run_id: &str, runs_dir: &Path) -> PathBuf {
    let preferred = runs_dir.join(format!("{run_id}.sock"));
    if preferred.as_os_str().len() <= MAX_SOCKET_PATH_LEN {
        return preferred;
    }

    tracing::warn!(
        run_id,
        preferred = %preferred.display(),
        "preferred event bus socket path exceeds the Unix socket path limit; \
         falling back to a short path under the OS temp directory"
    );
    std::env::temp_dir().join(format!("warden-{run_id}.sock"))
}

/// A run's Event Bus: owns the Unix socket listener and the broadcast
/// channel every accepted connection is subscribed to. Dropping this stops
/// new subscribers from being accepted (the socket file itself is only
/// removed by [`EventBus::bind`] of a *subsequent* run reusing the same
/// path, mirroring `warden_gated::relay::bind`'s stale-file handling) but
/// does not disconnect subscribers already attached -- they keep receiving
/// whatever was already broadcast to them.
pub struct EventBus {
    socket_path: PathBuf,
    sender: broadcast::Sender<RunEventRecord>,
}

impl EventBus {
    /// Binds a fresh listener at `<runs_dir>/<run_id>.sock`, creating
    /// `runs_dir` if needed and removing a stale socket file left over from
    /// a previous run with the same id first (a Unix socket path can't be
    /// re-bound while the old inode still exists). Hardens the socket file
    /// to `0600` right after bind -- `bind(2)` creates it with the
    /// umask-derived default mode, which is not narrow enough on its own
    /// (ADR-0008, mirrors `warden_gated::relay::bind`'s identical hardening
    /// for its own socket).
    ///
    /// Spawns the accept loop as a background task so publishing never has
    /// to wait on a subscriber connecting.
    pub async fn bind(run_id: &str, runs_dir: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(runs_dir).await?;
        let socket_path = resolve_socket_path(run_id, runs_dir);
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        if socket_path.exists() {
            tokio::fs::remove_file(&socket_path).await?;
        }

        let listener = UnixListener::bind(&socket_path)?;

        #[cfg(unix)]
        harden_socket_permissions(&socket_path).await?;

        let (sender, _receiver) = broadcast::channel(CHANNEL_CAPACITY);
        tokio::spawn(accept_loop(listener, sender.clone()));

        Ok(Self {
            socket_path,
            sender,
        })
    }

    /// Broadcasts `record` to every currently connected subscriber.
    /// Best-effort: `broadcast::Sender::send` only errors when there are
    /// zero receivers at all (every subscriber has disconnected, or none
    /// has ever connected), which is an entirely ordinary, expected state
    /// for a run nobody is currently attached to watching -- not a failure
    /// worth surfacing to the orchestrator (ADR-0008: a disconnected/absent
    /// subscriber must never affect the publisher).
    pub fn publish(&self, record: &RunEventRecord) {
        let _ = self.sender.send(record.clone());
    }

    /// The socket path this bus is bound to (`<runs_dir>/<run_id>.sock`),
    /// for logging/diagnostics.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

/// Restricts `socket_path` to owner-only read/write (`0600`), matching the
/// permission ADR-0008 mandates and the identical hardening
/// `warden_gated::relay::bind` already applies to its own socket.
#[cfg(unix)]
async fn harden_socket_permissions(socket_path: &Path) -> Result<()> {
    let mut permissions = tokio::fs::metadata(socket_path).await?.permissions();
    permissions.set_mode(0o600);
    tokio::fs::set_permissions(socket_path, permissions).await?;
    Ok(())
}

/// Accepts connections forever, spawning one forwarding task per subscriber.
/// Never reads from an accepted connection (see module docs) -- a
/// subscriber has nothing to say to the bus, only something to receive from
/// it.
async fn accept_loop(listener: UnixListener, sender: broadcast::Sender<RunEventRecord>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let receiver = sender.subscribe();
                tokio::spawn(forward_to_subscriber(stream, receiver));
            }
            Err(error) => {
                tracing::error!(%error, "event bus: failed to accept a subscriber connection");
                break;
            }
        }
    }
}

/// Forwards every broadcast event to `stream` as one line of JSON
/// (NDJSON, the same wire convention `warden_core::parse_findings` already
/// uses for agent stdout) until the subscriber disconnects or falls behind
/// far enough to be dropped by the broadcast channel.
async fn forward_to_subscriber(
    mut stream: UnixStream,
    mut receiver: broadcast::Receiver<RunEventRecord>,
) {
    loop {
        match receiver.recv().await {
            Ok(record) => {
                let line = match serde_json::to_string(&record) {
                    Ok(line) => line,
                    Err(error) => {
                        tracing::error!(%error, "event bus: failed to encode an event for a subscriber");
                        continue;
                    }
                };
                if stream.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if stream.write_all(b"\n").await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                // ADR-0008: the publisher is never blocked by a slow
                // subscriber -- this subscriber missed `skipped` events and
                // simply continues from the next one, rather than the
                // publish side ever waiting on it.
                tracing::warn!(skipped, "event bus: a subscriber lagged and missed events");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use warden_core::RunEvent;

    fn sample_record(id: &str) -> RunEventRecord {
        RunEventRecord {
            id: id.to_string(),
            run_id: "run-1".to_string(),
            event: RunEvent::CycleStarted { cycle_number: 1 },
            created_at: "2026-07-12T00:00:00+00:00".to_string(),
        }
    }

    #[tokio::test]
    async fn bind_creates_a_socket_restricted_to_owner_only_read_write() {
        let dir = TempDir::new().unwrap();
        let bus = EventBus::bind("run-1", dir.path()).await.unwrap();

        #[cfg(unix)]
        {
            let mode = std::fs::metadata(bus.socket_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "event bus socket must be owner-only read/write"
            );
        }
    }

    #[tokio::test]
    async fn a_subscriber_connected_before_publish_receives_the_event() {
        let dir = TempDir::new().unwrap();
        let bus = EventBus::bind("run-1", dir.path()).await.unwrap();

        let mut client = UnixStream::connect(bus.socket_path()).await.unwrap();
        // Give the accept loop a moment to register the subscription before
        // publishing -- otherwise `publish` could race ahead of `subscribe`.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let record = sample_record("event-1");
        bus.publish(&record);

        let mut reader = BufReader::new(&mut client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let received: RunEventRecord = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(received, record);
    }

    #[tokio::test]
    async fn every_connected_subscriber_receives_the_same_event() {
        let dir = TempDir::new().unwrap();
        let bus = EventBus::bind("run-1", dir.path()).await.unwrap();

        let mut client_a = UnixStream::connect(bus.socket_path()).await.unwrap();
        let mut client_b = UnixStream::connect(bus.socket_path()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let record = sample_record("event-1");
        bus.publish(&record);

        for client in [&mut client_a, &mut client_b] {
            let mut reader = BufReader::new(&mut *client);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let received: RunEventRecord = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(received, record);
        }
    }

    /// ADR-0008: "un abonné qui se déconnecte ne doit jamais bloquer
    /// l'orchestrateur" -- publishing with zero subscribers (none ever
    /// connected) must succeed without hanging or erroring the caller.
    #[tokio::test]
    async fn publish_with_no_subscribers_does_not_block_or_panic() {
        let dir = TempDir::new().unwrap();
        let bus = EventBus::bind("run-1", dir.path()).await.unwrap();

        bus.publish(&sample_record("event-1"));
    }

    /// Read-only enforcement: nothing this module does ever consumes bytes
    /// written by a subscriber. Writing to the socket from the client side
    /// must have no effect the orchestrator could observe -- there is no
    /// API on `EventBus` that could even expose it.
    #[tokio::test]
    async fn a_subscriber_writing_to_the_socket_has_no_effect_on_the_bus() {
        let dir = TempDir::new().unwrap();
        let bus = EventBus::bind("run-1", dir.path()).await.unwrap();

        let mut client = UnixStream::connect(bus.socket_path()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // A malicious/confused subscriber attempting to send a command --
        // the bus has no read path to ever notice this happened.
        client
            .write_all(b"{\"command\":\"abort\"}\n")
            .await
            .unwrap();

        let record = sample_record("event-1");
        bus.publish(&record);

        let mut reader = BufReader::new(&mut client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let received: RunEventRecord = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(
            received, record,
            "the bus must still deliver the real event regardless of what the subscriber wrote"
        );
    }

    #[tokio::test]
    async fn bind_replaces_a_stale_socket_file_left_by_a_previous_run() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("run-1.sock");
        std::fs::write(&socket_path, b"not a real socket").unwrap();

        let bus = EventBus::bind("run-1", dir.path()).await;
        assert!(bus.is_ok());
    }

    /// A `--warden-home` deep enough to push `<runs_dir>/<run_id>.sock` past
    /// the Unix socket path limit must still let the run bind an Event Bus
    /// -- observed for real against tempdir-based test fixtures on macOS,
    /// not just a theoretical case (see `resolve_socket_path`'s docs).
    #[tokio::test]
    async fn bind_succeeds_even_when_the_preferred_path_would_exceed_the_socket_path_limit() {
        let dir = TempDir::new().unwrap();
        let deeply_nested_runs_dir = dir.path().join("a".repeat(120));
        let run_id = "11111111-1111-1111-1111-111111111111";

        let bus = EventBus::bind(run_id, &deeply_nested_runs_dir)
            .await
            .unwrap();

        assert!(
            bus.socket_path().as_os_str().len() <= MAX_SOCKET_PATH_LEN,
            "fallback socket path is still too long: {}",
            bus.socket_path().display()
        );
    }

    #[test]
    fn resolve_socket_path_prefers_runs_dir_when_short_enough() {
        let runs_dir = Path::new("/tmp/warden/runs");
        let run_id = "11111111-1111-1111-1111-111111111111";

        let resolved = resolve_socket_path(run_id, runs_dir);

        assert_eq!(resolved, runs_dir.join(format!("{run_id}.sock")));
    }

    #[test]
    fn resolve_socket_path_falls_back_to_temp_dir_when_runs_dir_is_too_long() {
        let runs_dir = PathBuf::from(format!("/tmp/{}", "a".repeat(200)));
        let run_id = "11111111-1111-1111-1111-111111111111";

        let resolved = resolve_socket_path(run_id, &runs_dir);

        assert_eq!(
            resolved,
            std::env::temp_dir().join(format!("warden-{run_id}.sock"))
        );
        assert!(resolved.as_os_str().len() <= MAX_SOCKET_PATH_LEN);
    }
}
