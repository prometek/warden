//! Event Bus socket path resolution (ADR-0008), shared verbatim between the
//! publisher (`warden::event_bus`) and the subscriber (`warden_tui::subscriber`).
//!
//! Unlike `warden::db`/`warden_tui::db`'s deliberate per-crate duplication
//! (ADR-0006 -- keeping two crates' *SQL query correctness* independently
//! re-verified), this is not business logic either side should re-derive
//! independently: it's a single wire-addressing rule that both ends of the
//! same protocol must agree on byte-for-byte. If the two crates ever computed
//! this differently, a long `--warden-home` path would silently make the
//! publisher bind one socket and the subscriber look for another --
//! downgrading a live run to a history-only attach with no error at all.
//! Kept here, in the one crate both already depend on, instead.

use std::path::{Path, PathBuf};

/// Conservative usable length for `sockaddr_un.sun_path`. The real limit is
/// platform-specific (104 bytes total including the NUL terminator on
/// macOS/BSD, 108 on Linux) -- 100 leaves headroom on the tighter of the
/// two rather than cutting it exactly at the boundary.
pub const MAX_SOCKET_PATH_LEN: usize = 100;

/// Resolves where a run's Event Bus socket lives: the ADR-0008-mandated
/// `<runs_dir>/<run_id>.sock` when that fits within [`MAX_SOCKET_PATH_LEN`],
/// otherwise a short, deterministic path under the OS temp directory keyed
/// by `run_id` alone.
///
/// `runs_dir` comes from `--warden-home` (user-controlled, and `~/.warden`
/// itself may already sit under a deep sandboxed/containerized home
/// directory) concatenated with a UUID run id -- comfortably within limits
/// for a typical `$HOME`, but not guaranteed for every deployment. Binding
/// would otherwise fail outright with an opaque `EINVAL`/`SUN_LEN` OS error;
/// falling back to a short, still run-id-keyed path keeps the Event Bus
/// working rather than silently disabling Phase 8 observability for anyone
/// whose `--warden-home` happens to resolve to a long path.
pub fn resolve_socket_path(run_id: &str, runs_dir: &Path) -> PathBuf {
    resolve_named_socket_path(run_id, runs_dir, "sock", "warden")
}

/// Resolves where a run's **reverse** CI-result socket lives (issue
/// #15/ADR-0011): `warden` binds this one, `warden-gated` connects to it to
/// deliver the terminal `CiResultMessage`. Deliberately a distinct address
/// from [`resolve_socket_path`]'s Event Bus socket (`.ci.sock` vs. `.sock`)
/// even though both are keyed by the same `run_id` -- they're two unrelated
/// protocols (broadcast/read-only observability vs. a single-shot,
/// bidirectional-process-boundary result delivery) that must never be
/// confused for one another; a `warden-tui` subscriber accidentally
/// connecting to this one would just hang forever waiting for bytes nothing
/// ever sends down that path.
pub fn resolve_ci_result_socket_path(run_id: &str, runs_dir: &Path) -> PathBuf {
    resolve_named_socket_path(run_id, runs_dir, "ci.sock", "wci")
}

/// Shared implementation behind [`resolve_socket_path`] and
/// [`resolve_ci_result_socket_path`]: same preferred-path-under-`runs_dir`,
/// same short-deterministic-temp-dir fallback once it would exceed
/// [`MAX_SOCKET_PATH_LEN`] -- only the file suffix and temp-dir prefix
/// differ between the two socket kinds.
fn resolve_named_socket_path(
    run_id: &str,
    runs_dir: &Path,
    suffix: &str,
    temp_prefix: &str,
) -> PathBuf {
    let preferred = runs_dir.join(format!("{run_id}.{suffix}"));
    if preferred.as_os_str().len() <= MAX_SOCKET_PATH_LEN {
        return preferred;
    }
    std::env::temp_dir().join(format!("{temp_prefix}-{run_id}.{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn resolve_socket_path_is_deterministic_for_the_same_inputs() {
        // The whole point of sharing this function between `warden` and
        // `warden-tui` is that calling it twice with identical inputs from
        // two different processes must agree -- this pins that down.
        let runs_dir = PathBuf::from(format!("/tmp/{}", "b".repeat(200)));
        let run_id = "22222222-2222-2222-2222-222222222222";

        assert_eq!(
            resolve_socket_path(run_id, &runs_dir),
            resolve_socket_path(run_id, &runs_dir)
        );
    }

    // ---- resolve_ci_result_socket_path (issue #15/ADR-0011) ----------------

    #[test]
    fn resolve_ci_result_socket_path_prefers_runs_dir_when_short_enough() {
        let runs_dir = Path::new("/tmp/warden/runs");
        let run_id = "11111111-1111-1111-1111-111111111111";

        let resolved = resolve_ci_result_socket_path(run_id, runs_dir);

        assert_eq!(resolved, runs_dir.join(format!("{run_id}.ci.sock")));
    }

    #[test]
    fn resolve_ci_result_socket_path_falls_back_to_temp_dir_when_runs_dir_is_too_long() {
        let runs_dir = PathBuf::from(format!("/tmp/{}", "a".repeat(200)));
        let run_id = "11111111-1111-1111-1111-111111111111";

        let resolved = resolve_ci_result_socket_path(run_id, &runs_dir);

        assert_eq!(
            resolved,
            std::env::temp_dir().join(format!("wci-{run_id}.ci.sock"))
        );
        assert!(resolved.as_os_str().len() <= MAX_SOCKET_PATH_LEN);
    }

    #[test]
    fn ci_result_socket_path_never_collides_with_the_event_bus_socket_path() {
        let runs_dir = Path::new("/tmp/warden/runs");
        let run_id = "11111111-1111-1111-1111-111111111111";

        assert_ne!(
            resolve_socket_path(run_id, runs_dir),
            resolve_ci_result_socket_path(run_id, runs_dir)
        );
    }
}
