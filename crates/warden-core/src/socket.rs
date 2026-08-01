//! Event Bus socket path resolution, shared verbatim between the publisher (`warden::event_bus`)
//! and the subscriber (`warden_tui::subscriber`).

use std::path::{Path, PathBuf};

/// Conservative usable length for `sockaddr_un.sun_path`.
pub const MAX_SOCKET_PATH_LEN: usize = 100;

/// Resolves where a run's Event Bus socket lives.
pub fn resolve_socket_path(run_id: &str, runs_dir: &Path) -> PathBuf {
    resolve_named_socket_path(run_id, runs_dir, "sock", "warden")
}

/// Resolves where a run's **reverse** CI-result socket lives: `warden` binds this one, `warden-
/// gated` connects to it to deliver the terminal `CiResultMessage`.
pub fn resolve_ci_result_socket_path(run_id: &str, runs_dir: &Path) -> PathBuf {
    resolve_named_socket_path(run_id, runs_dir, "ci.sock", "wci")
}

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
        let runs_dir = PathBuf::from(format!("/tmp/{}", "b".repeat(200)));
        let run_id = "22222222-2222-2222-2222-222222222222";

        assert_eq!(
            resolve_socket_path(run_id, &runs_dir),
            resolve_socket_path(run_id, &runs_dir)
        );
    }

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
