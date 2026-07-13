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
use warden_core::{resolve_socket_path, RunEventRecord};

use crate::error::Result;

/// Bound on how many not-yet-delivered events a single slow subscriber can
/// lag behind before older ones are dropped for it (see module docs). Named
/// rather than left at whatever `broadcast::channel`'s caller happens to
/// pick, since it directly trades off "how much history survives a
/// subscriber hiccup" against "how much memory a stalled subscriber can pin".
const CHANNEL_CAPACITY: usize = 256;

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
    /// `runs_dir` if needed (owner-only, `0700` -- see [`create_private_dir`])
    /// and removing a stale socket file left over from a previous run with
    /// the same id first (a Unix socket path can't be re-bound while the old
    /// inode still exists). Hardens the socket file to `0600` right after
    /// bind -- `bind(2)` creates it with the umask-derived default mode,
    /// which is not narrow enough on its own (ADR-0008, mirrors
    /// `warden_gated::relay::bind`'s identical hardening for its own socket).
    ///
    /// Spawns the accept loop as a background task so publishing never has
    /// to wait on a subscriber connecting.
    pub async fn bind(run_id: &str, runs_dir: &Path) -> Result<Self> {
        create_private_dir(runs_dir).await?;
        let socket_path = resolve_socket_path(run_id, runs_dir);
        if let Some(parent) = socket_path.parent() {
            // Only ever `runs_dir` itself (already hardened above) or, in
            // the long-path fallback, the OS temp directory -- which this
            // must *not* be narrowed to `0700`, since it's shared with every
            // other process/user on the machine, not owned by `warden`.
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

impl Drop for EventBus {
    /// Best-effort removal of the socket file once the run that owned it
    /// ends. Run ids are UUIDs, so [`EventBus::bind`]'s own stale-file
    /// replacement (which only fires on an *exact* path collision) would
    /// practically never clean these up on its own -- without this,
    /// `<runs_dir>` would accumulate one orphaned socket per completed run
    /// forever. A failure here (already gone, a benign race with another
    /// process) is not worth surfacing: the run has already ended either
    /// way, and this is cleanup, not a step anything downstream depends on.
    fn drop(&mut self) {
        match std::fs::remove_file(&self.socket_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::debug!(
                    socket = %self.socket_path.display(),
                    %error,
                    "failed to remove event bus socket file on shutdown"
                );
            }
        }
    }
}

/// Creates `dir` (if needed) and restricts it to owner-only access (`0700`)
/// -- used for `runs_dir` itself, which `warden` owns exclusively. Closes
/// the brief window between `mkdir` (umask-derived default mode, typically
/// `0755`) and an explicit `chmod` during which the directory would
/// otherwise be group/world-readable, listing every run id (and, however
/// briefly, any socket already bound inside it) to other local users.
#[cfg(unix)]
async fn create_private_dir(dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let mut permissions = tokio::fs::metadata(dir).await?.permissions();
    permissions.set_mode(0o700);
    tokio::fs::set_permissions(dir, permissions).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn create_private_dir(dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    Ok(())
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
///
/// An `accept` failure is logged and does not stop the loop: `accept` errors
/// (`EMFILE`, a dropped-before-fully-established connection, ...) are
/// typically transient, and a single hiccup permanently killing live attach
/// for the rest of the run would be far worse than logging and trying again.
/// A short delay precedes the retry so a *persistent* failure (e.g. the
/// process is out of file descriptors) can't turn into a tight busy-loop.
async fn accept_loop(listener: UnixListener, sender: broadcast::Sender<RunEventRecord>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let receiver = sender.subscribe();
                tokio::spawn(forward_to_subscriber(stream, receiver));
            }
            Err(error) => {
                tracing::error!(%error, "event bus: failed to accept a subscriber connection; retrying");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
            }
        }
    }
}

/// See [`accept_loop`]'s docs on why a retry backoff exists at all.
const ACCEPT_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

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

    /// ADR-0008: "un abonné qui se déconnecte ne doit jamais bloquer
    /// l'orchestrateur" -- a subscriber that connects but never reads its
    /// socket must never slow down `publish`, and a second, well-behaved
    /// subscriber must still receive events promptly regardless of the
    /// first one falling behind. Publishes well beyond
    /// [`CHANNEL_CAPACITY`] with sizeable payloads, so a lagging
    /// subscriber's un-drained OS socket buffer would genuinely fill (and
    /// its per-connection `forward_to_subscriber` task would stall on
    /// `write`) if this property did not hold.
    #[tokio::test]
    async fn a_lagging_subscriber_never_blocks_publish_or_other_subscribers() {
        let dir = TempDir::new().unwrap();
        let bus = EventBus::bind("run-1", dir.path()).await.unwrap();

        // Connected but deliberately never read from.
        let _lagging_client = UnixStream::connect(bus.socket_path()).await.unwrap();
        let mut healthy_client = UnixStream::connect(bus.socket_path()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let large_description = "x".repeat(4096);
        let large_record = |id: String| RunEventRecord {
            id,
            run_id: "run-1".to_string(),
            event: RunEvent::FindingRaised {
                cycle_number: 1,
                source: "reviewer".to_string(),
                severity: "info".to_string(),
                file: None,
                description: large_description.clone(),
                action: None,
            },
            created_at: "2026-07-12T00:00:00+00:00".to_string(),
        };

        let publish_started = tokio::time::Instant::now();
        // Comfortably beyond both the broadcast channel's bounded capacity
        // and any reasonable OS Unix-socket buffer size.
        for i in 0..(CHANNEL_CAPACITY * 4) {
            bus.publish(&large_record(format!("event-{i}")));
        }
        assert!(
            publish_started.elapsed() < std::time::Duration::from_secs(2),
            "publish must never block on a subscriber that isn't reading"
        );

        // The well-behaved subscriber must still receive events promptly --
        // not starved by the lagging one.
        let mut reader = BufReader::new(&mut healthy_client);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await
        .expect("healthy subscriber must not be starved by the lagging one")
        .unwrap();
        let received: RunEventRecord = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(received.run_id, "run-1");
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
            bus.socket_path().as_os_str().len() <= warden_core::MAX_SOCKET_PATH_LEN,
            "fallback socket path is still too long: {}",
            bus.socket_path().display()
        );
    }

    // `resolve_socket_path`/`MAX_SOCKET_PATH_LEN` themselves are tested in
    // `warden_core::socket` now, the single shared implementation both this
    // module and `warden_tui::subscriber` call into (see that module's
    // docs) -- no longer duplicated here.

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_restricts_runs_dir_itself_to_owner_only_access() {
        let dir = TempDir::new().unwrap();
        let runs_dir = dir.path().join("runs");
        let _bus = EventBus::bind("run-1", &runs_dir).await.unwrap();

        let mode = std::fs::metadata(&runs_dir).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "runs_dir must be owner-only, closing the mkdir/chmod window on the socket inside it"
        );
    }

    /// Issue #8 review, item 6: run ids are UUIDs, so `bind`'s own
    /// stale-file replacement (path-exact) practically never fires to clean
    /// up a finished run's socket -- without `Drop`, `runs_dir` would
    /// accumulate one orphaned file per run forever.
    #[tokio::test]
    async fn dropping_the_event_bus_removes_its_socket_file() {
        let dir = TempDir::new().unwrap();
        let bus = EventBus::bind("run-1", dir.path()).await.unwrap();
        let socket_path = bus.socket_path().to_path_buf();
        assert!(socket_path.exists(), "precondition: socket file exists");

        drop(bus);

        assert!(
            !socket_path.exists(),
            "socket file must be removed once the EventBus that owns it is dropped"
        );
    }

    /// Dropping an `EventBus` whose socket file has already been removed by
    /// something else must not panic -- `Drop` can't return a `Result`, so
    /// this has to be handled internally as a no-op.
    #[tokio::test]
    async fn dropping_the_event_bus_after_its_socket_was_already_removed_does_not_panic() {
        let dir = TempDir::new().unwrap();
        let bus = EventBus::bind("run-1", dir.path()).await.unwrap();
        std::fs::remove_file(bus.socket_path()).unwrap();

        drop(bus);
    }
}
