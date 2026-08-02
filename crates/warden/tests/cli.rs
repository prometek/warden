use std::path::Path;
use std::process::Command as SyncCommand;

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;

fn init_repo() -> TempDir {
    let repo = TempDir::new().unwrap();
    for args in [
        &["init", "--quiet"][..],
        &["config", "user.email", "test@warden.local"],
        &["config", "user.name", "warden-test"],
    ] {
        assert!(SyncCommand::new("git")
            .current_dir(repo.path())
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(repo.path().join("README.md"), "seed\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "--quiet", "-m", "seed"]);
    repo
}

fn git(repo: &Path, args: &[&str]) {
    assert!(SyncCommand::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .unwrap()
        .success());
}

fn write_workflow(repo: &Path, raw: &str) {
    std::fs::create_dir_all(repo.join(".warden")).unwrap();
    std::fs::write(repo.join(".warden/workflow.yaml"), raw).unwrap();
}

fn warden() -> Command {
    Command::new(env!("CARGO_BIN_EXE_warden"))
}

#[test]
fn missing_workflow_is_actionable_error_not_implicit_default() {
    let repo = init_repo();
    let home = TempDir::new().unwrap();
    warden()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "change",
            "--warden-home",
            home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stderr(contains("workflow file is required"))
        .stderr(contains(".warden/workflow.yaml"));
}

#[test]
fn removed_role_specific_cycle_flags_are_rejected() {
    let repo = init_repo();
    warden()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "change",
            "--tool",
            "claude",
            "--max-review-cycles",
            "2",
        ])
        .assert()
        .failure()
        .stderr(contains("unexpected argument '--max-review-cycles'"));
}

#[test]
fn command_only_graph_converges_without_any_agent_definition() {
    let repo = init_repo();
    let home = TempDir::new().unwrap();
    write_workflow(
        repo.path(),
        r#"
name: checks
entry: format
steps:
  format:
    type: command
    run: test -f README.md
    on_clean: converged
    on_blocking: failed
    on_error: failed
"#,
    );
    warden()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "validate",
            "--warden-home",
            home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));
}

#[cfg(unix)]
#[test]
fn arbitrary_agent_step_receives_uniform_context_and_may_create_commit() {
    use std::os::unix::fs::PermissionsExt;

    let repo = init_repo();
    let state_home = TempDir::new().unwrap();
    let agent_home = TempDir::new().unwrap();
    let bin = TempDir::new().unwrap();
    write_workflow(
        repo.path(),
        r#"
name: arbitrary
entry: implementation
steps:
  implementation:
    type: agent
    agent: writer
    on_clean: converged
    on_blocking: implementation
    on_error: failed
"#,
    );
    std::fs::create_dir_all(repo.path().join(".warden/agents")).unwrap();
    std::fs::write(
        repo.path().join(".warden/agents/writer.md"),
        "---\ntools: Read, Write, Edit, Bash\n---\nImplement requested change.\n",
    )
    .unwrap();
    let script = bin.path().join("claude");
    std::fs::write(
        &script,
        r#"#!/bin/sh
set -eu
cat > "$HOME/payload.json"
echo done > result.txt
git add result.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m result
printf '%s\n' '{"result":""}'
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap()
    );

    warden()
        .env("PATH", path)
        .env("HOME", agent_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "create result",
            "--warden-home",
            state_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let payload = std::fs::read_to_string(agent_home.path().join("payload.json")).unwrap();
    let parsed = warden_core::parse_agent_input_message(&payload).unwrap();
    assert_eq!(parsed.role.as_str(), "implementation");
    assert_eq!(parsed.intent, "create result");
    assert!(!parsed.current_commit.is_empty());
}
