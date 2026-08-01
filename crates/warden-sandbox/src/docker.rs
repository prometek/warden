//! [`DockerSandbox`]: containerised execution isolation -- the second [`crate::Sandbox`] backend,
//! alongside [`crate::LocalSandbox`]'s host-process parity default.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use crate::drain::drain_and_wait;
use crate::error::{Result, SandboxError};
use crate::{Command, ExecuteOptions, Execution, ExecutionResult, Sandbox, SandboxId, SandboxSpec};

/// The `docker` CLI binary, resolved via `PATH` -- same convention [`crate::LocalSandbox`] uses for
/// the agent's own `command.program`.
const DOCKER_BIN: &str = "docker";

const CONTAINER_HOME: &str = "/root";

/// What every [`DockerSandbox::execute`] call needs beyond the per-command [`Command`] itself: the
/// image to run, and the two host paths this backend's mounts are built from.
pub struct DockerConfig {
    pub image: String,
    pub repo_path: PathBuf,
    pub claude_config_dir: PathBuf,
}

pub struct DockerSandbox {
    config: DockerConfig,
    sandboxes: Mutex<HashMap<SandboxId, PathBuf>>,
}

impl DockerSandbox {
    pub fn new(config: DockerConfig) -> Self {
        Self {
            config,
            sandboxes: Mutex::new(HashMap::new()),
        }
    }

    fn cwd_for(&self, id: &SandboxId) -> Result<PathBuf> {
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| SandboxError::UnknownSandbox { id: id.clone() })
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId> {
        let id = SandboxId::new(format!("warden-{}", uuid::Uuid::new_v4()));
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), spec.cwd);
        Ok(id)
    }

    async fn execute<'a>(
        &'a self,
        id: &'a SandboxId,
        command: Command,
        options: ExecuteOptions<'a>,
    ) -> Result<Execution<'a>> {
        let cwd = self.cwd_for(id)?;
        let container_name = id.to_string();

        let host_worktree = canonicalize_host_path(&cwd)?;
        let host_repo_git = canonicalize_host_path(&self.config.repo_path.join(".git"))?;
        let host_claude_dir = self.config.claude_config_dir.canonicalize().map_err(|_| {
            SandboxError::DockerUnavailable {
                reason: format!(
                    "host Claude config directory {} does not exist; `--isolation docker` \
                     requires the host to already be logged into `claude` (run `claude` at \
                     least once outside docker first)",
                    self.config.claude_config_dir.display()
                ),
            }
        })?;

        let forwarded_env = resolve_forwarded_env(&command.env_allowlist, &command.program);
        let argv = build_docker_run_argv(
            &container_name,
            &self.config.image,
            &host_worktree,
            &host_repo_git,
            &host_claude_dir,
            CONTAINER_HOME,
            &forwarded_env,
            &command.program,
            &command.args,
        );

        let mut cmd = tokio::process::Command::new(DOCKER_BIN);
        cmd.args(&argv)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = cmd.spawn().map_err(|source| SandboxError::Spawn {
            program: DOCKER_BIN.to_string(),
            source,
        })?;
        let pid = child.id();

        let program_label = format!("docker (container {container_name})");
        let stdin_payload = command.stdin;
        let cancel = options.cancel;
        let on_stdout_line = options.on_stdout_line;

        Ok(Execution::new(
            pid,
            drain_and_wait_with_container_cleanup(
                child,
                program_label,
                stdin_payload,
                cancel,
                on_stdout_line,
                container_name,
            ),
        ))
    }

    async fn destroy(&self, id: SandboxId) -> Result<()> {
        let removed = self
            .sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id)
            .is_some();
        if !removed {
            return Ok(());
        }
        remove_container(&id.to_string()).await
    }
}

async fn drain_and_wait_with_container_cleanup(
    child: Child,
    program: String,
    stdin_payload: Option<String>,
    cancel: CancellationToken,
    on_stdout_line: Option<&(dyn Fn(&str) + Send + Sync)>,
    container_name: String,
) -> Result<ExecutionResult> {
    let result = drain_and_wait(child, program, stdin_payload, cancel, on_stdout_line).await;
    if matches!(result, Err(SandboxError::Cancelled { .. })) {
        force_remove_container(&container_name).await;
    }
    let result = result?;

    if let Some(reason) = classify_docker_startup_failure(result.exit_code, &result.stderr) {
        return Err(SandboxError::DockerUnavailable { reason });
    }

    Ok(result)
}

/// Distinguishes a `docker run` startup failure (daemon down, image missing) from a normal -- if
/// non-zero -- exit of whatever ran *inside* the container.
fn classify_docker_startup_failure(exit_code: i32, stderr: &str) -> Option<String> {
    if exit_code != 125 {
        return None;
    }
    if stderr.contains("Cannot connect to the Docker daemon") {
        return Some(format!(
            "the docker daemon is not reachable -- start Docker and retry. docker's own stderr: {}",
            stderr.trim()
        ));
    }
    if stderr.contains("Unable to find image") || stderr.contains("No such image") {
        return Some(format!(
            "the docker image was not found -- build it via \
             crates/warden-sandbox/docker/Dockerfile (see \
             crates/warden-sandbox/docker/README.md) and retry. docker's own stderr: {}",
            stderr.trim()
        ));
    }
    None
}

async fn force_remove_container(container_name: &str) {
    let output = tokio::process::Command::new(DOCKER_BIN)
        .args(["rm", "-f", container_name])
        .stdin(Stdio::null())
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) if is_benign_removal_race(&String::from_utf8_lossy(&output.stderr)) => {}
        Ok(output) => {
            tracing::warn!(
                container_name,
                status = ?output.status,
                stderr = %String::from_utf8_lossy(&output.stderr),
                "docker rm -f exited non-zero during cancellation cleanup"
            );
        }
        Err(error) => {
            tracing::warn!(container_name, %error, "failed to run docker rm -f during cancellation cleanup");
        }
    }
}

async fn remove_container(container_name: &str) -> Result<()> {
    let output = tokio::process::Command::new(DOCKER_BIN)
        .args(["rm", "-f", container_name])
        .output()
        .await
        .map_err(|source| SandboxError::Spawn {
            program: DOCKER_BIN.to_string(),
            source,
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_benign_removal_race(&stderr) {
        return Ok(());
    }
    Err(SandboxError::DockerUnavailable {
        reason: format!("`docker rm -f {container_name}` failed: {}", stderr.trim()),
    })
}

fn is_benign_removal_race(stderr: &str) -> bool {
    stderr.contains("No such container")
        || stderr.contains("already in progress")
        || stderr.contains("is being removed")
}

fn resolve_forwarded_env(env_allowlist: &[String], program: &str) -> Vec<(String, String)> {
    env_allowlist
        .iter()
        .filter(|name| name.as_str() != "HOME")
        .filter_map(|name| match std::env::var(name) {
            Ok(value) => Some((name.clone(), value)),
            Err(_) => {
                tracing::warn!(
                    var = name,
                    program,
                    "adapter-requested environment variable is not set in warden's own \
                     process environment; the container will run without it"
                );
                None
            }
        })
        .collect()
}

/// Resolves a host path this backend needs to bind-mount to its canonical, absolute form.
fn canonicalize_host_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .map_err(|source| SandboxError::DockerUnavailable {
            reason: format!(
                "cannot resolve host path {} for a docker bind mount: {source}",
                path.display()
            ),
        })
}

#[allow(clippy::too_many_arguments)]
fn build_docker_run_argv(
    container_name: &str,
    image: &str,
    host_worktree: &Path,
    host_repo_git: &Path,
    host_claude_dir: &Path,
    container_home: &str,
    forwarded_env: &[(String, String)],
    program: &str,
    args: &[String],
) -> Vec<String> {
    let worktree = host_worktree.display().to_string();
    let repo_git = host_repo_git.display().to_string();
    let claude_dir = host_claude_dir.display().to_string();

    let mut argv = vec![
        "run".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        container_name.to_string(),
        "-i".to_string(),
        "--mount".to_string(),
        format!("type=bind,source={worktree},target={worktree}"),
        "--mount".to_string(),
        format!("type=bind,source={repo_git},target={repo_git}"),
        "--mount".to_string(),
        format!("type=bind,source={claude_dir},target={container_home}/.claude,readonly"),
        "-e".to_string(),
        format!("HOME={container_home}"),
        // Neutralise git's "dubious ownership" guard *inside the container*.
        "-e".to_string(),
        "GIT_CONFIG_COUNT=1".to_string(),
        "-e".to_string(),
        "GIT_CONFIG_KEY_0=safe.directory".to_string(),
        "-e".to_string(),
        "GIT_CONFIG_VALUE_0=*".to_string(),
    ];
    for (name, value) in forwarded_env {
        argv.push("-e".to_string());
        argv.push(format!("{name}={value}"));
    }
    argv.push("-w".to_string());
    argv.push(worktree);
    argv.push(image.to_string());
    argv.push(program.to_string());
    argv.extend(args.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const TEST_IMAGE: &str = "alpine:latest";

    #[test]
    fn argv_contains_the_worktree_and_repo_git_mounts_at_identical_host_paths() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );

        assert!(argv.contains(
            &"type=bind,source=/host/worktrees/coder,target=/host/worktrees/coder".to_string()
        ));
        assert!(
            argv.contains(&"type=bind,source=/host/repo/.git,target=/host/repo/.git".to_string())
        );
    }

    #[test]
    fn argv_uses_mount_not_the_colon_ambiguous_v_flag() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );

        assert!(!argv.contains(&"-v".to_string()));
        assert_eq!(argv.iter().filter(|arg| *arg == "--mount").count(), 3);
    }

    #[test]
    fn argv_mounts_claude_config_read_only_under_the_container_home() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );

        assert!(argv.contains(
            &"type=bind,source=/host/home/.claude,target=/root/.claude,readonly".to_string()
        ));
    }

    #[test]
    fn argv_always_sets_home_to_the_container_home() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );

        let home_entries: Vec<&String> = argv
            .iter()
            .zip(argv.iter().skip(1))
            .filter(|(flag, _)| flag.as_str() == "-e")
            .map(|(_, value)| value)
            .filter(|value| value.starts_with("HOME="))
            .collect();
        assert_eq!(home_entries, vec!["HOME=/root"]);
    }

    #[test]
    fn argv_never_mounts_ssh_aws_or_gh_config() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );
        let joined = argv.join(" ");

        assert!(!joined.contains(".ssh"));
        assert!(!joined.contains(".aws"));
        assert!(!joined.contains(".config/gh"));
        assert!(!joined.contains(".env"));
    }

    #[test]
    fn argv_disables_gits_dubious_ownership_guard_via_env_config() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );

        assert!(argv.contains(&"GIT_CONFIG_COUNT=1".to_string()));
        assert!(argv.contains(&"GIT_CONFIG_KEY_0=safe.directory".to_string()));
        assert!(argv.contains(&"GIT_CONFIG_VALUE_0=*".to_string()));
    }

    #[test]
    fn argv_sets_no_network_flag_default_bridge() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );

        assert!(!argv.contains(&"--network".to_string()));
    }

    #[test]
    fn argv_sets_working_directory_and_program_args_after_the_image() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &["--output-format".to_string(), "json".to_string()],
        );

        let w_index = argv.iter().position(|arg| arg == "-w").unwrap();
        assert_eq!(argv[w_index + 1], "/host/worktrees/coder");

        let image_index = argv
            .iter()
            .position(|arg| arg == "warden-agent:latest")
            .unwrap();
        assert_eq!(argv[image_index + 1], "claude");
        assert_eq!(argv[image_index + 2], "--output-format");
        assert_eq!(argv[image_index + 3], "json");
    }

    #[test]
    fn argv_forwards_only_the_resolved_env_pairs_given() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[("USER".to_string(), "alice".to_string())],
            "claude",
            &[],
        );

        assert!(argv.contains(&"USER=alice".to_string()));
    }

    #[test]
    fn resolve_forwarded_env_forwards_a_set_allowlisted_variable() {
        let expected = std::env::var("CARGO_MANIFEST_DIR")
            .expect("precondition: cargo test sets CARGO_MANIFEST_DIR");
        let forwarded = resolve_forwarded_env(&["CARGO_MANIFEST_DIR".to_string()], "claude");
        assert_eq!(
            forwarded,
            vec![("CARGO_MANIFEST_DIR".to_string(), expected)]
        );
    }

    #[test]
    fn resolve_forwarded_env_always_strips_home() {
        let forwarded = resolve_forwarded_env(&["HOME".to_string(), "USER".to_string()], "claude");
        assert!(forwarded.iter().all(|(name, _)| name != "HOME"));
    }

    #[test]
    fn resolve_forwarded_env_skips_a_variable_missing_from_this_process_own_environment() {
        let forwarded =
            resolve_forwarded_env(&["THIS_VAR_DOES_NOT_EXIST_ANYWHERE".to_string()], "claude");
        assert!(forwarded.is_empty());
    }

    #[test]
    fn classifies_exit_125_with_daemon_unreachable_stderr_as_docker_unavailable() {
        let reason = classify_docker_startup_failure(
            125,
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker \
             daemon running?",
        );
        assert!(reason.is_some());
        assert!(reason.unwrap().contains("start Docker"));
    }

    #[test]
    fn classifies_exit_125_with_missing_image_stderr_as_docker_unavailable() {
        let reason = classify_docker_startup_failure(
            125,
            "Unable to find image 'warden-agent:latest' locally\ndocker: Error response from \
             daemon: pull access denied for warden-agent, repository does not exist or may \
             require 'docker login'",
        );
        assert!(reason.is_some());
        assert!(reason.unwrap().contains("Dockerfile"));
    }

    #[test]
    fn does_not_classify_exit_125_without_a_docker_level_stderr_marker() {
        assert!(classify_docker_startup_failure(125, "some unrelated agent error").is_none());
    }

    #[test]
    fn does_not_classify_a_non_125_exit_code_even_with_a_docker_level_looking_stderr() {
        assert!(classify_docker_startup_failure(1, "Unable to find image 'x' locally").is_none());
    }

    #[test]
    fn treats_already_gone_and_removal_races_as_benign() {
        assert!(is_benign_removal_race(
            "Error: No such container: warden-abc123"
        ));
        assert!(is_benign_removal_race(
            "Error response from daemon: removal of container warden-abc123 is already in \
             progress"
        ));
        assert!(is_benign_removal_race(
            "Error response from daemon: container warden-abc123 is being removed"
        ));
    }

    #[test]
    fn does_not_treat_an_unrelated_failure_as_benign() {
        assert!(!is_benign_removal_race(
            "Error response from daemon: permission denied"
        ));
    }

    fn config(dir: &TempDir) -> DockerConfig {
        DockerConfig {
            image: "warden-agent:latest".to_string(),
            repo_path: dir.path().to_path_buf(),
            claude_config_dir: dir.path().to_path_buf(),
        }
    }

    #[tokio::test]
    async fn create_mints_a_warden_prefixed_id() {
        let dir = TempDir::new().unwrap();
        let sandbox = DockerSandbox::new(config(&dir));
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();
        assert!(id.to_string().starts_with("warden-"));
    }

    #[tokio::test]
    async fn destroy_is_idempotent_for_an_id_that_was_never_created() {
        let dir = TempDir::new().unwrap();
        let sandbox = DockerSandbox::new(config(&dir));
        assert!(sandbox
            .destroy(SandboxId::new("warden-never-created"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn execute_with_an_unknown_sandbox_id_reports_a_typed_error() {
        let dir = TempDir::new().unwrap();
        let sandbox = DockerSandbox::new(config(&dir));
        let bogus_id = SandboxId::new("warden-bogus");

        let result = sandbox
            .execute(
                &bogus_id,
                Command {
                    program: "true".to_string(),
                    args: Vec::new(),
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await;
        assert!(matches!(result, Err(SandboxError::UnknownSandbox { .. })));
    }

    #[tokio::test]
    async fn execute_reports_a_typed_error_when_the_claude_config_dir_is_missing() {
        let dir = TempDir::new().unwrap();
        let mut cfg = config(&dir);
        cfg.claude_config_dir = dir.path().join("does-not-exist");
        let sandbox = DockerSandbox::new(cfg);
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let result = sandbox
            .execute(
                &id,
                Command {
                    program: "true".to_string(),
                    args: Vec::new(),
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await;
        assert!(matches!(
            result,
            Err(SandboxError::DockerUnavailable { .. })
        ));
    }

    async fn docker_daemon_available() -> bool {
        tokio::process::Command::new(DOCKER_BIN)
            .arg("info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn init_repo_with_worktree_and_claude_dir() -> (TempDir, PathBuf, TempDir) {
        let repo = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(repo.path())
                .args(args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.email", "test@warden.local"]);
        run(&["config", "user.name", "warden-test"]);
        std::fs::write(
            repo.path().join("README.md"),
            "warden docker sandbox test\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "--quiet", "-m", "initial commit"]);
        run(&[
            "remote",
            "add",
            "origin",
            "https://example.invalid/nonexistent/repo.git",
        ]);

        let worktree = repo.path().join("worktree");
        run(&["worktree", "add", "--detach", worktree.to_str().unwrap()]);

        let claude_dir = TempDir::new().unwrap();
        std::fs::write(claude_dir.path().join(".credentials.json"), "{}").unwrap();

        (repo, worktree, claude_dir)
    }

    #[tokio::test]
    async fn execute_reports_docker_unavailable_when_the_image_does_not_exist() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }

        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: "warden-agent-image-that-does-not-exist-anywhere:latest".to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
        });
        let id = sandbox
            .create(SandboxSpec {
                cwd: worktree.clone(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                Command {
                    program: "true".to_string(),
                    args: Vec::new(),
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        let result = execution.wait().await;

        assert!(
            matches!(result, Err(SandboxError::DockerUnavailable { .. })),
            "expected a typed DockerUnavailable for a missing image, got {result:?}"
        );

        sandbox.destroy(id).await.unwrap();
    }

    #[tokio::test]
    async fn e2e_git_push_origin_fails_inside_the_container_no_credentials_mounted() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }

        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: TEST_IMAGE.to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
        });
        let id = sandbox
            .create(SandboxSpec {
                cwd: worktree.clone(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                Command {
                    program: "sh".to_string(),
                    args: vec![
                        "-c".to_string(),
                        "apk add --no-cache git >/dev/null 2>&1 && git push origin HEAD"
                            .to_string(),
                    ],
                    env_allowlist: vec!["HOME".to_string(), "USER".to_string()],
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert_ne!(
            outcome.exit_code, 0,
            "git push origin must fail: stderr was {}",
            outcome.stderr
        );

        sandbox.destroy(id).await.unwrap();
    }

    #[tokio::test]
    async fn e2e_host_ssh_aws_gh_and_env_are_not_reachable_inside_the_container() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }

        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: TEST_IMAGE.to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
        });
        let id = sandbox
            .create(SandboxSpec {
                cwd: worktree.clone(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                Command {
                    program: "sh".to_string(),
                    args: vec![
                        "-c".to_string(),
                        "test -e /root/.ssh && echo SSH_FOUND; test -e /root/.aws && echo AWS_FOUND; \
                         test -e /root/.config/gh && echo GH_FOUND; test -e /root/.env && echo ENV_FOUND; \
                         test -e $HOME/.claude/.credentials.json && echo CLAUDE_FOUND; exit 0"
                            .to_string(),
                    ],
                    env_allowlist: vec!["HOME".to_string()],
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert!(
            !outcome.stdout.contains("SSH_FOUND"),
            "host ~/.ssh must not be reachable inside the container: stdout was {}",
            outcome.stdout
        );
        assert!(
            !outcome.stdout.contains("AWS_FOUND"),
            "host ~/.aws must not be reachable inside the container: stdout was {}",
            outcome.stdout
        );
        assert!(
            !outcome.stdout.contains("GH_FOUND"),
            "host ~/.config/gh must not be reachable inside the container: stdout was {}",
            outcome.stdout
        );
        assert!(
            !outcome.stdout.contains("ENV_FOUND"),
            "host ~/.env must not be reachable inside the container: stdout was {}",
            outcome.stdout
        );
        assert!(
            outcome.stdout.contains("CLAUDE_FOUND"),
            "~/.claude must be reachable (read-only) at the container HOME: stdout was {}",
            outcome.stdout
        );

        sandbox.destroy(id).await.unwrap();
    }

    #[tokio::test]
    async fn destroy_leaves_no_container_behind() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }

        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: TEST_IMAGE.to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
        });
        let id = sandbox
            .create(SandboxSpec {
                cwd: worktree.clone(),
            })
            .await
            .unwrap();
        let container_name = id.to_string();

        let execution = sandbox
            .execute(
                &id,
                Command {
                    program: "true".to_string(),
                    args: Vec::new(),
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        execution.wait().await.unwrap();

        sandbox.destroy(id).await.unwrap();

        assert!(
            !container_exists(&container_name).await,
            "no `{container_name}` container should remain after destroy"
        );
    }

    #[tokio::test]
    async fn cancelling_an_execution_leaves_no_container_behind() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }

        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: TEST_IMAGE.to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
        });
        let id = sandbox
            .create(SandboxSpec {
                cwd: worktree.clone(),
            })
            .await
            .unwrap();
        let container_name = id.to_string();
        let cancel = CancellationToken::new();

        let execution = sandbox
            .execute(
                &id,
                Command {
                    program: "sleep".to_string(),
                    args: vec!["30".to_string()],
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions {
                    cancel: cancel.clone(),
                    on_stdout_line: None,
                },
            )
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        cancel.cancel();
        let result = execution.wait().await;
        assert!(matches!(result, Err(SandboxError::Cancelled { .. })));

        let mut still_there = container_exists(&container_name).await;
        for _ in 0..20 {
            if !still_there {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            still_there = container_exists(&container_name).await;
        }
        assert!(
            !still_there,
            "no `{container_name}` container should remain after cancellation"
        );

        sandbox.destroy(id).await.unwrap();
    }

    async fn container_exists(container_name: &str) -> bool {
        let output = tokio::process::Command::new(DOCKER_BIN)
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("name=^{container_name}$"),
                "--format",
                "{{.Names}}",
            ])
            .output()
            .await
            .expect("spawn docker ps");
        !String::from_utf8_lossy(&output.stdout).trim().is_empty()
    }
}
