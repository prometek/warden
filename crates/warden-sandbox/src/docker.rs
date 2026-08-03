//! Docker isolation for agent processes.
//!
//! Worktree and common `.git` are mounted read-write. Host `~/.claude` is
//! mounted read-only but remains readable, so default bridge networking lets a
//! malicious agent exfiltrate Claude credentials and repository data. Other
//! host credentials are not mounted. Host-side git commands disable hooks.
//!
//! Containers keep only `CAP_DAC_OVERRIDE` for writable host-owned bind mounts,
//! forbid privilege escalation, limit process count, and carry run labels for
//! crash recovery. Docker daemon and host kernel remain trusted; network, CPU,
//! and memory are not constrained.

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
const MANAGED_LABEL: &str = "io.warden.managed";
const RUN_ID_LABEL: &str = "io.warden.run-id";
const CONTAINER_PIDS_LIMIT: &str = "256";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerEgressConfig {
    pub network: String,
    pub proxy: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockerRunOptions {
    pub cpus: Option<String>,
    pub memory: Option<String>,
    pub egress: Option<DockerEgressConfig>,
}

/// What every [`DockerSandbox::execute`] call needs beyond the per-command [`Command`] itself: the
/// image to run, and the two host paths this backend's mounts are built from.
pub struct DockerConfig {
    pub image: String,
    pub repo_path: PathBuf,
    pub claude_config_dir: PathBuf,
    pub run_options: DockerRunOptions,
}

pub struct DockerSandbox {
    config: DockerConfig,
    sandboxes: Mutex<HashMap<SandboxId, DockerSandboxEntry>>,
}

#[derive(Clone)]
struct DockerSandboxEntry {
    cwd: PathBuf,
    run_id: Option<String>,
}

impl DockerSandbox {
    pub fn new(config: DockerConfig) -> Self {
        Self {
            config,
            sandboxes: Mutex::new(HashMap::new()),
        }
    }

    fn entry_for(&self, id: &SandboxId) -> Result<DockerSandboxEntry> {
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| SandboxError::UnknownSandbox { id: id.clone() })
    }

    fn create_entry(&self, spec: SandboxSpec, run_id: Option<&str>) -> SandboxId {
        let id = SandboxId::new(format!("warden-{}", uuid::Uuid::new_v4()));
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id.clone(),
                DockerSandboxEntry {
                    cwd: spec.cwd,
                    run_id: run_id.map(str::to_owned),
                },
            );
        id
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId> {
        Ok(self.create_entry(spec, None))
    }

    async fn create_for_run(&self, spec: SandboxSpec, run_id: &str) -> Result<SandboxId> {
        Ok(self.create_entry(spec, Some(run_id)))
    }

    async fn execute<'a>(
        &'a self,
        id: &'a SandboxId,
        command: Command,
        options: ExecuteOptions<'a>,
    ) -> Result<Execution<'a>> {
        let entry = self.entry_for(id)?;
        let container_name = id.to_string();

        validate_internal_egress_network(&self.config.run_options).await?;
        let host_worktree = canonicalize_host_path(&entry.cwd)?;
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
        let argv = build_docker_run_argv_with_options(
            &container_name,
            entry.run_id.as_deref(),
            &self.config.image,
            &host_worktree,
            &host_repo_git,
            &host_claude_dir,
            CONTAINER_HOME,
            &self.config.run_options,
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

async fn validate_internal_egress_network(options: &DockerRunOptions) -> Result<()> {
    let Some(egress) = &options.egress else {
        return Ok(());
    };
    let output = tokio::process::Command::new(DOCKER_BIN)
        .args([
            "network",
            "inspect",
            "--format",
            "{{.Internal}}",
            &egress.network,
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| SandboxError::Spawn {
            program: DOCKER_BIN.to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(SandboxError::DockerUnavailable {
            reason: format!(
                "cannot inspect configured egress network {:?}: {}",
                egress.network,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    if String::from_utf8_lossy(&output.stdout).trim() != "true" {
        return Err(SandboxError::DockerUnavailable {
            reason: format!(
                "configured egress network {:?} is not internal; refusing direct Internet access",
                egress.network
            ),
        });
    }
    Ok(())
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

/// Removes containers labeled for a crashed run.
pub async fn reclaim_run_containers(run_id: &str) -> Result<usize> {
    let managed_filter = format!("label={MANAGED_LABEL}=true");
    let run_filter = format!("label={RUN_ID_LABEL}={run_id}");
    let output = tokio::process::Command::new(DOCKER_BIN)
        .args([
            "ps",
            "--all",
            "--quiet",
            "--filter",
            &managed_filter,
            "--filter",
            &run_filter,
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| SandboxError::Spawn {
            program: DOCKER_BIN.to_string(),
            source,
        })?;

    if !output.status.success() {
        return Err(SandboxError::DockerUnavailable {
            reason: format!(
                "cannot list containers for run {run_id}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }

    let container_ids: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    for container_id in &container_ids {
        remove_container(container_id).await?;
    }
    Ok(container_ids.len())
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
#[cfg(test)]
fn build_docker_run_argv(
    container_name: &str,
    run_id: Option<&str>,
    image: &str,
    host_worktree: &Path,
    host_repo_git: &Path,
    host_claude_dir: &Path,
    container_home: &str,
    forwarded_env: &[(String, String)],
    program: &str,
    args: &[String],
) -> Vec<String> {
    build_docker_run_argv_with_options(
        container_name,
        run_id,
        image,
        host_worktree,
        host_repo_git,
        host_claude_dir,
        container_home,
        &DockerRunOptions::default(),
        forwarded_env,
        program,
        args,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_docker_run_argv_with_options(
    container_name: &str,
    run_id: Option<&str>,
    image: &str,
    host_worktree: &Path,
    host_repo_git: &Path,
    host_claude_dir: &Path,
    container_home: &str,
    options: &DockerRunOptions,
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
        "--label".to_string(),
        format!("{MANAGED_LABEL}=true"),
        "--cap-drop".to_string(),
        "ALL".to_string(),
        "--cap-add".to_string(),
        "DAC_OVERRIDE".to_string(),
        "--security-opt".to_string(),
        "no-new-privileges=true".to_string(),
        "--pids-limit".to_string(),
        CONTAINER_PIDS_LIMIT.to_string(),
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
    if let Some(run_id) = run_id {
        argv.push("--label".to_string());
        argv.push(format!("{RUN_ID_LABEL}={run_id}"));
    }
    if let Some(cpus) = &options.cpus {
        argv.push("--cpus".to_string());
        argv.push(cpus.clone());
    }
    if let Some(memory) = &options.memory {
        argv.push("--memory".to_string());
        argv.push(memory.clone());
    }
    if let Some(egress) = &options.egress {
        argv.push("--network".to_string());
        argv.push(egress.network.clone());
    }
    for (name, value) in forwarded_env {
        argv.push("-e".to_string());
        argv.push(format!("{name}={value}"));
    }
    if let Some(egress) = &options.egress {
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            argv.push("-e".to_string());
            argv.push(format!("{name}={}", egress.proxy));
        }
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
    use std::process::Command as SyncCommand;
    use tempfile::TempDir;

    const TEST_IMAGE: &str =
        "alpine:3.24.1@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b";
    const TEST_AGENT_IMAGE: &str = "warden-agent:0.1.0";

    #[test]
    fn argv_contains_the_worktree_and_repo_git_mounts_at_identical_host_paths() {
        let argv = build_docker_run_argv(
            "warden-test",
            None,
            TEST_AGENT_IMAGE,
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
    fn argv_labels_tracked_containers_and_applies_security_baseline() {
        let argv = build_docker_run_argv(
            "warden-test",
            Some("run-123"),
            TEST_AGENT_IMAGE,
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );
        let has_pair = |flag: &str, value: &str| {
            argv.windows(2)
                .any(|pair| pair[0] == flag && pair[1] == value)
        };

        assert!(has_pair("--label", "io.warden.managed=true"));
        assert!(has_pair("--label", "io.warden.run-id=run-123"));
        assert!(has_pair("--cap-drop", "ALL"));
        assert!(has_pair("--cap-add", "DAC_OVERRIDE"));
        assert!(has_pair("--security-opt", "no-new-privileges=true"));
        assert!(has_pair("--pids-limit", "256"));
    }

    #[test]
    fn argv_uses_mount_not_the_colon_ambiguous_v_flag() {
        let argv = build_docker_run_argv(
            "warden-test",
            None,
            TEST_AGENT_IMAGE,
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
            None,
            TEST_AGENT_IMAGE,
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
            None,
            TEST_AGENT_IMAGE,
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
            None,
            TEST_AGENT_IMAGE,
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
            None,
            TEST_AGENT_IMAGE,
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
            None,
            TEST_AGENT_IMAGE,
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );

        assert!(!argv.contains(&"--network".to_string()));
        assert!(!argv.contains(&"--cpus".to_string()));
        assert!(!argv.contains(&"--memory".to_string()));
    }

    #[test]
    fn argv_applies_configured_limits_and_fail_closed_egress() {
        let options = DockerRunOptions {
            cpus: Some("2.5".to_string()),
            memory: Some("4g".to_string()),
            egress: Some(DockerEgressConfig {
                network: "warden-egress".to_string(),
                proxy: "http://warden-proxy:3128".to_string(),
            }),
        };
        let argv = build_docker_run_argv_with_options(
            "warden-test",
            None,
            TEST_AGENT_IMAGE,
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &options,
            &[(
                "HTTPS_PROXY".to_string(),
                "http://untrusted-host-value:8080".to_string(),
            )],
            "claude",
            &[],
        );
        let has_pair = |flag: &str, value: &str| {
            argv.windows(2)
                .any(|pair| pair[0] == flag && pair[1] == value)
        };

        assert!(has_pair("--cpus", "2.5"));
        assert!(has_pair("--memory", "4g"));
        assert!(has_pair("--network", "warden-egress"));
        let https_proxy_values: Vec<_> = argv
            .windows(2)
            .filter(|pair| pair[0] == "-e" && pair[1].starts_with("HTTPS_PROXY="))
            .map(|pair| pair[1].as_str())
            .collect();
        assert_eq!(
            https_proxy_values.last(),
            Some(&"HTTPS_PROXY=http://warden-proxy:3128")
        );
        assert!(argv.contains(&"HTTP_PROXY=http://warden-proxy:3128".to_string()));
        assert!(argv.contains(&"http_proxy=http://warden-proxy:3128".to_string()));
        assert!(argv.contains(&"https_proxy=http://warden-proxy:3128".to_string()));
    }

    #[test]
    fn argv_sets_working_directory_and_program_args_after_the_image() {
        let argv = build_docker_run_argv(
            "warden-test",
            None,
            TEST_AGENT_IMAGE,
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

        let image_index = argv.iter().position(|arg| arg == TEST_AGENT_IMAGE).unwrap();
        assert_eq!(argv[image_index + 1], "claude");
        assert_eq!(argv[image_index + 2], "--output-format");
        assert_eq!(argv[image_index + 3], "json");
    }

    #[test]
    fn argv_forwards_only_the_resolved_env_pairs_given() {
        let argv = build_docker_run_argv(
            "warden-test",
            None,
            TEST_AGENT_IMAGE,
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
            "Unable to find image 'warden-agent:0.1.0' locally\ndocker: Error response from \
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
            image: TEST_AGENT_IMAGE.to_string(),
            repo_path: dir.path().to_path_buf(),
            claude_config_dir: dir.path().to_path_buf(),
            run_options: DockerRunOptions::default(),
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

    async fn docker_image_available(image: &str) -> bool {
        if let Some((name, digest)) = image.split_once('@') {
            let repository = name.split(':').next().unwrap_or(name);
            return tokio::process::Command::new(DOCKER_BIN)
                .args([
                    "images",
                    "--digests",
                    "--format",
                    "{{.Repository}}@{{.Digest}}",
                    repository,
                ])
                .output()
                .await
                .map(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout)
                            .lines()
                            .any(|entry| entry == format!("{repository}@{digest}"))
                })
                .unwrap_or(false);
        }
        tokio::process::Command::new(DOCKER_BIN)
            .args(["image", "inspect", image])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false)
    }

    struct DockerFixtureGuard {
        container: Option<String>,
        network: Option<String>,
    }

    impl Drop for DockerFixtureGuard {
        fn drop(&mut self) {
            if let Some(container) = &self.container {
                let _ = SyncCommand::new(DOCKER_BIN)
                    .args(["rm", "--force", container])
                    .output();
            }
            if let Some(network) = &self.network {
                let _ = SyncCommand::new(DOCKER_BIN)
                    .args(["network", "rm", network])
                    .output();
            }
        }
    }

    fn create_internal_proxy_fixture() -> (DockerFixtureGuard, String, String) {
        let suffix = uuid::Uuid::new_v4();
        let network = format!("warden-egress-test-{suffix}");
        let proxy = format!("warden-proxy-test-{suffix}");
        let network_output = SyncCommand::new(DOCKER_BIN)
            .args(["network", "create", "--internal", &network])
            .output()
            .unwrap();
        assert!(
            network_output.status.success(),
            "failed to create internal Docker network: {}",
            String::from_utf8_lossy(&network_output.stderr)
        );
        let mut guard = DockerFixtureGuard {
            container: None,
            network: Some(network.clone()),
        };
        let proxy_output = SyncCommand::new(DOCKER_BIN)
            .args([
                "run",
                "--detach",
                "--name",
                &proxy,
                "--network",
                &network,
                TEST_IMAGE,
                "sh",
                "-c",
                "while true; do printf 'proxy-ok\\n' | nc -l -p 3128; done",
            ])
            .output()
            .unwrap();
        assert!(
            proxy_output.status.success(),
            "failed to create proxy fixture: {}",
            String::from_utf8_lossy(&proxy_output.stderr)
        );
        guard.container = Some(proxy.clone());
        let ready = (0..40).any(|_| {
            let listening = SyncCommand::new(DOCKER_BIN)
                .args(["exec", &proxy, "sh", "-c", "netstat -ltn | grep -q 3128"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !listening {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            listening
        });
        assert!(ready, "proxy fixture never listened on port 3128");
        (guard, network, proxy)
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
            image: "warden-agent-image-that-does-not-exist-anywhere:missing".to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
            run_options: DockerRunOptions::default(),
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
    async fn configured_egress_uses_an_internal_network_and_reaches_its_proxy() {
        if !docker_daemon_available().await || !docker_image_available(TEST_IMAGE).await {
            eprintln!("skipping: Docker daemon or pinned test image unavailable");
            return;
        }

        let (_fixture, network, proxy) = create_internal_proxy_fixture();
        let proxy_url = format!("http://{proxy}:3128");
        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: TEST_IMAGE.to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
            run_options: DockerRunOptions {
                cpus: Some("0.5".to_string()),
                memory: Some("64m".to_string()),
                egress: Some(DockerEgressConfig {
                    network,
                    proxy: proxy_url.clone(),
                }),
            },
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
                        format!(
                            "test \"$HTTP_PROXY\" = \"{proxy_url}\" && \
                             test \"$HTTPS_PROXY\" = \"{proxy_url}\" && nc {proxy} 3128"
                        ),
                    ],
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert_eq!(outcome.exit_code, 0, "stderr was {}", outcome.stderr);
        assert_eq!(outcome.stdout.trim(), "proxy-ok");
        sandbox.destroy(id).await.unwrap();
    }

    #[tokio::test]
    async fn configured_egress_rejects_a_non_internal_network() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no Docker daemon reachable");
            return;
        }

        let network = format!("warden-open-network-test-{}", uuid::Uuid::new_v4());
        let network_output = SyncCommand::new(DOCKER_BIN)
            .args(["network", "create", &network])
            .output()
            .unwrap();
        assert!(
            network_output.status.success(),
            "failed to create non-internal Docker network: {}",
            String::from_utf8_lossy(&network_output.stderr)
        );
        let _fixture = DockerFixtureGuard {
            container: None,
            network: Some(network.clone()),
        };
        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: TEST_IMAGE.to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
            run_options: DockerRunOptions {
                cpus: None,
                memory: None,
                egress: Some(DockerEgressConfig {
                    network,
                    proxy: "http://proxy:3128".to_string(),
                }),
            },
        });
        let id = sandbox.create(SandboxSpec { cwd: worktree }).await.unwrap();
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

        match result {
            Err(SandboxError::DockerUnavailable { reason }) => {
                assert!(reason.contains("is not internal"));
            }
            Err(error) => panic!("expected DockerUnavailable, got {error}"),
            Ok(_) => panic!("non-internal network must be rejected"),
        }
        sandbox.destroy(id.clone()).await.unwrap();
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
            run_options: DockerRunOptions::default(),
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
            run_options: DockerRunOptions::default(),
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
            run_options: DockerRunOptions::default(),
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
    async fn crash_recovery_removes_a_container_after_its_docker_client_dies() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }
        if !docker_image_available(TEST_IMAGE).await {
            eprintln!("skipping: {TEST_IMAGE} is not available locally");
            return;
        }

        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: TEST_IMAGE.to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
            run_options: DockerRunOptions::default(),
        });
        let run_id = format!("recovery-{}", uuid::Uuid::new_v4());
        let id = sandbox
            .create_for_run(
                SandboxSpec {
                    cwd: worktree.clone(),
                },
                &run_id,
            )
            .await
            .unwrap();
        let container_name = id.to_string();
        let execution = sandbox
            .execute(
                &id,
                Command {
                    program: "sleep".to_string(),
                    args: vec!["30".to_string()],
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await
            .unwrap();

        wait_for_container_to_appear(&container_name).await;

        drop(execution);
        assert_eq!(reclaim_run_containers(&run_id).await.unwrap(), 1);
        assert!(!container_exists(&container_name).await);
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
            run_options: DockerRunOptions::default(),
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

    /// Blocks until `docker run` has registered `container_name` with the daemon.
    ///
    /// `execute` returns as soon as the `docker` child is spawned, so the
    /// container is not visible to `docker ps` yet. Registration is normally a
    /// couple of hundred milliseconds, but a cold daemon has been measured at
    /// ~1.8s, and a loaded CI runner is slower still — hence a budget generous
    /// enough that only a genuine failure can exhaust it. The happy path still
    /// returns on the first poll that sees the container.
    async fn wait_for_container_to_appear(container_name: &str) {
        const POLL: std::time::Duration = std::time::Duration::from_millis(100);
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

        let deadline = std::time::Instant::now() + BUDGET;
        loop {
            if container_exists(container_name).await {
                return;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(POLL).await;
        }

        // `docker run`'s own stderr is unreachable here: the child is owned by
        // the unpolled `Execution` future. Report what the daemon can still be
        // asked about, so a real failure is diagnosable from the CI log alone.
        let listing = tokio::process::Command::new(DOCKER_BIN)
            .args(["ps", "--all", "--format", "{{.Names}}\t{{.Status}}"])
            .output()
            .await
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_else(|error| format!("<`docker ps` failed: {error}>"));
        panic!(
            "container `{container_name}` never appeared within {BUDGET:?}; \
             `docker ps --all` reports:\n{listing}"
        );
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
