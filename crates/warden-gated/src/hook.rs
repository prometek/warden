//! The `post-receive` hook installed into the local bare gate repo
//! (ADR-0002/ADR-0006, issue #3: "hook post-receive minimal (relais
//! uniquement, aucune logique métier dans le hook)"). The script this
//! module generates does exactly one thing: exec the already-installed
//! `warden-gated notify` subcommand with stdin passed through untouched.
//! No parsing, no decision -- both happen later, in Rust, inside the
//! `serve` daemon (`notification.rs`, `gate.rs`), where they're testable.

use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::fs;

use crate::error::Result;

/// Builds the `post-receive` hook script content. `warden_gated_bin` is the
/// absolute path to the installed `warden-gated` binary; `socket_path` is
/// where the `serve` daemon listens. Both are baked into the script at
/// install time so the hook itself needs no environment/config lookup of
/// its own -- one less thing that could drift or be tampered with between
/// install and invocation.
pub fn post_receive_script(warden_gated_bin: &Path, socket_path: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # Installed by `warden-gated init-bare`. Relay only -- see\n\
         # ADR-0002/ADR-0006 and code-standards.md \"Inter-process\n\
         # Communication\": no business logic belongs in this file. Whatever\n\
         # git wrote to stdin (one \"<old-sha> <new-sha> <ref>\" line per\n\
         # updated ref) is forwarded verbatim; parsing and the push decision\n\
         # both happen inside warden-gated itself.\n\
         exec {bin} notify --socket {socket}\n",
        bin = shell_quote(&warden_gated_bin.display().to_string()),
        socket = shell_quote(&socket_path.display().to_string()),
    )
}

/// Single-quotes `value` for embedding in the generated `/bin/sh` script,
/// escaping any literal single quotes it contains -- paths are attacker-
/// adjacent input here (an admin-controlled install path, not untrusted at
/// runtime, but still worth not word-splitting/globbing by accident).
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Installs the hook into `bare_repo_path/hooks/post-receive`, executable
/// (`0755` on Unix -- git refuses to run a non-executable hook).
pub async fn install(
    bare_repo_path: &Path,
    warden_gated_bin: &Path,
    socket_path: &Path,
) -> Result<()> {
    let hooks_dir = bare_repo_path.join("hooks");
    fs::create_dir_all(&hooks_dir).await?;
    let hook_path = hooks_dir.join("post-receive");
    let script = post_receive_script(warden_gated_bin, socket_path);
    fs::write(&hook_path, script).await?;

    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&hook_path).await?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook_path, permissions).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn generated_script_execs_notify_with_both_paths_quoted() {
        let script = post_receive_script(
            Path::new("/usr/local/bin/warden-gated"),
            Path::new("/home/user/.warden/gated.sock"),
        );
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains(
            "exec '/usr/local/bin/warden-gated' notify --socket '/home/user/.warden/gated.sock'\n"
        ));
    }

    #[test]
    fn shell_quoting_escapes_embedded_single_quotes() {
        let script = post_receive_script(
            Path::new("/it's/here/warden-gated"),
            Path::new("/tmp/gated.sock"),
        );
        assert!(script.contains("'/it'\\''s/here/warden-gated'"));
    }

    #[tokio::test]
    async fn install_writes_an_executable_hook_file() {
        let bare_repo = TempDir::new().unwrap();
        let bin = Path::new("/usr/local/bin/warden-gated");
        let socket = Path::new("/tmp/gated.sock");

        install(bare_repo.path(), bin, socket).await.unwrap();

        let hook_path = bare_repo.path().join("hooks").join("post-receive");
        let contents = tokio::fs::read_to_string(&hook_path).await.unwrap();
        assert!(contents.contains("notify --socket"));

        #[cfg(unix)]
        {
            let mode = tokio::fs::metadata(&hook_path)
                .await
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "hook must be executable");
        }
    }
}
