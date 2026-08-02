use std::path::{Path, PathBuf};
use std::process::Command as SyncCommand;

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;
use warden_core::{FindingSource, RunState};

fn init_test_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let run = |args: &[&str]| {
        let status = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "--quiet"]);
    run(&["config", "user.email", "test@warden.local"]);
    run(&["config", "user.name", "warden-test"]);
    std::fs::write(dir.path().join("README.md"), "warden test repo\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "--quiet", "-m", "initial commit"]);
    dir
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

#[cfg(unix)]
fn write_fake_tool(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = write_script(dir, name, body);
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn write_agent_definition(repo: &Path, role: &str, frontmatter: &str, system_prompt: &str) {
    let agents_dir = repo.join(".warden").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join(format!("{role}.md")),
        format!("---\n{frontmatter}---\n\n{system_prompt}\n"),
    )
    .unwrap();
}

fn write_user_config_agent_definition(
    xdg_config_home: &Path,
    role: &str,
    frontmatter: &str,
    system_prompt: &str,
) {
    let agents_dir = xdg_config_home.join("warden").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join(format!("{role}.md")),
        format!("---\n{frontmatter}---\n\n{system_prompt}\n"),
    )
    .unwrap();
}

#[cfg(unix)]
fn write_fake_claude(dir: &Path, coder_body: &str, reviewer_body: &str, tester_body: &str) {
    let script = format!(
        r#"#!/bin/sh
set -e
stdin_file=$(mktemp)
cat > "$stdin_file"
WARDEN_RESULT_FILE=$(mktemp)
export WARDEN_RESULT_FILE
: > "$WARDEN_RESULT_FILE"

if grep -q '"role":"coder"' "$stdin_file"; then
{coder_body}
elif grep -q '"role":"reviewer"' "$stdin_file"; then
{reviewer_body}
else
{tester_body}
fi

result=$(cat "$WARDEN_RESULT_FILE")
rm -f "$WARDEN_RESULT_FILE" "$stdin_file"
escaped=$(printf '%s' "$result" | python3 -c 'import json,sys; sys.stdout.write(json.dumps(sys.stdin.read()))')
printf '{{"type":"result","subtype":"success","is_error":false,"result":%s}}\n' "$escaped"
"#
    );
    write_fake_tool(dir, "claude", &script);
}

#[cfg(unix)]
fn write_fake_codex(dir: &Path, coder_body: &str, reviewer_body: &str, tester_body: &str) {
    let script = format!(
        r#"#!/bin/sh
set -e
stdin_file=$(mktemp)
cat > "$stdin_file"
WARDEN_RESULT_FILE=$(mktemp)
export WARDEN_RESULT_FILE
: > "$WARDEN_RESULT_FILE"

if grep -q '"role":"coder"' "$stdin_file"; then
{coder_body}
elif grep -q '"role":"reviewer"' "$stdin_file"; then
{reviewer_body}
else
{tester_body}
fi

result=$(cat "$WARDEN_RESULT_FILE")
rm -f "$WARDEN_RESULT_FILE" "$stdin_file"
escaped=$(printf '%s' "$result" | python3 -c 'import json,sys; sys.stdout.write(json.dumps(sys.stdin.read()))')
printf '{{"msg":{{"type":"agent_message","message":"working"}}}}\n'
printf '{{"msg":{{"type":"token_count","input_tokens":11,"output_tokens":22}}}}\n'
printf '{{"msg":{{"type":"task_complete","last_agent_message":%s}}}}\n' "$escaped"
"#
    );
    write_fake_tool(dir, "codex", &script);
}

#[cfg(unix)]
fn write_fake_mistral(dir: &Path, coder_body: &str, reviewer_body: &str, tester_body: &str) {
    let script = format!(
        r#"#!/bin/sh
set -e
stdin_file=$(mktemp)
cat > "$stdin_file"
WARDEN_RESULT_FILE=$(mktemp)
export WARDEN_RESULT_FILE
: > "$WARDEN_RESULT_FILE"

if grep -q '"role":"coder"' "$stdin_file"; then
{coder_body}
elif grep -q '"role":"reviewer"' "$stdin_file"; then
{reviewer_body}
else
{tester_body}
fi

cat "$WARDEN_RESULT_FILE"
rm -f "$WARDEN_RESULT_FILE" "$stdin_file"
"#
    );
    write_fake_tool(dir, "mistral", &script);
}

const FLIP_STATUS_CODER_BODY: &str = r#"
if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
    echo fixed > status.txt
else
    echo broken > status.txt
fi
git add status.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#;

const STATUS_GATED_REVIEWER_BODY: &str = r#"
if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
    printf '%s\n' '{"source":"reviewer","severity":"blocking","description":"status is broken"}' > "$WARDEN_RESULT_FILE"
fi
"#;

const NOOP_BODY: &str = "true";

const APPEND_NOTES_CODER_BODY: &str = r#"
echo hello >> notes.txt
git add notes.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#;

#[cfg(unix)]
fn path_with_fake_bin_first(fake_bin_dir: &Path) -> String {
    let real_path = std::env::var("PATH").unwrap_or_default();
    format!("{}:{real_path}", fake_bin_dir.display())
}

fn warden_command() -> (Command, TempDir) {
    let hermetic_home = TempDir::new().expect("tempdir");
    let mut cmd = Command::cargo_bin("warden").unwrap();
    cmd.env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("XDG_CONFIG_HOME", hermetic_home.path())
        .env("HOME", hermetic_home.path());
    (cmd, hermetic_home)
}

#[cfg(unix)]
fn write_fake_asciinema(dir: &Path) -> PathBuf {
    write_fake_tool(
        dir,
        "asciinema",
        r#"#!/bin/sh
for arg in "$@"; do
    output="$arg"
done
echo '{"version": 2, "width": 80, "height": 24, "timestamp": 0}' > "$output"
exit 0
"#,
    )
}

#[cfg(unix)]
fn write_fake_npx(dir: &Path) -> PathBuf {
    write_fake_tool(
        dir,
        "npx",
        r#"#!/bin/sh
mkdir -p test-results/example-spec
printf 'fake-png-bytes' > test-results/example-spec/screenshot.png
exit 0
"#,
    )
}

fn extract_run_id(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("could not find run id in stdout: {stdout:?}"))
        .to_string()
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_run_id_and_attach_command_are_printed_at_start_before_finished_without_v_flag() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "print run id at start",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();

    let started_idx = lines
        .iter()
        .position(|line| line.ends_with(" started"))
        .unwrap_or_else(|| panic!("no \"started\" line found in stdout: {stdout:?}"));
    let attach_idx = lines
        .iter()
        .position(|line| line.starts_with("attach: "))
        .unwrap_or_else(|| panic!("no \"attach:\" line found in stdout: {stdout:?}"));
    let finished_idx = lines
        .iter()
        .position(|line| line.contains(" finished: "))
        .unwrap_or_else(|| panic!("no \"finished:\" line found in stdout: {stdout:?}"));

    assert!(
        started_idx < attach_idx,
        "the \"started\" line must appear immediately before the \"attach:\" line: {stdout:?}"
    );
    assert!(
        attach_idx < finished_idx,
        "both the \"started\" and \"attach:\" lines must appear before the run finishes, not \
         only once it's already done: {stdout:?}"
    );

    let started_run_id = lines[started_idx]
        .strip_prefix("run ")
        .and_then(|rest| rest.strip_suffix(" started"))
        .unwrap_or_else(|| {
            panic!(
                "unexpected \"started\" line shape: {:?}",
                lines[started_idx]
            )
        });
    let finished_run_id = lines[finished_idx]
        .strip_prefix("run ")
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| {
            panic!(
                "unexpected \"finished\" line shape: {:?}",
                lines[finished_idx]
            )
        });

    let expected_attach_line = format!(
        "attach: warden-tui attach --run-id {started_run_id} --warden-home {}",
        warden_home.path().display()
    );
    assert_eq!(
        lines[attach_idx], expected_attach_line,
        "the attach command must be copy-pasteable verbatim, naming this exact run id and the \
         effective --warden-home"
    );

    assert_eq!(
        started_run_id, finished_run_id,
        "the \"started\" line and the \"finished\" line must report the exact same run id"
    );

    let pool = warden::db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let run = warden::db::get_run(&pool, started_run_id)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no `runs` row found for id {started_run_id}"));
    assert_eq!(run.id, started_run_id);
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_attach_command_shows_the_resolved_default_warden_home_when_flag_is_omitted() {
    let repo = init_test_repo();
    let fake_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", fake_home.path())
        .env("HOME", fake_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "resolved default warden-home",
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let expected_resolved_home = fake_home.path().join(".warden");
    let expected_fragment = format!("--warden-home {}", expected_resolved_home.display());

    assert!(
        stdout.contains(&expected_fragment),
        "expected the attach command to name the resolved default warden-home \
         ({expected_fragment:?}), got stdout: {stdout:?}"
    );

    assert!(
        expected_resolved_home.join("state.db").exists(),
        "the resolved default warden-home ({}) must be where this run's state.db actually is",
        expected_resolved_home.display()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_attach_command_shell_quotes_a_warden_home_containing_a_space() {
    let repo = init_test_repo();
    let warden_home_root = TempDir::new().unwrap();
    let warden_home = warden_home_root.path().join("My Warden Home");
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", &warden_home)
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "shell-quote a warden-home containing a space",
            "--warden-home",
            warden_home.to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    let started_line = stdout
        .lines()
        .find(|line| line.ends_with(" started"))
        .unwrap_or_else(|| panic!("no \"started\" line found in stdout: {stdout:?}"));
    let run_id = started_line
        .strip_prefix("run ")
        .and_then(|rest| rest.strip_suffix(" started"))
        .unwrap_or_else(|| panic!("unexpected \"started\" line shape: {started_line:?}"));

    let attach_line = stdout
        .lines()
        .find(|line| line.starts_with("attach: "))
        .unwrap_or_else(|| panic!("no \"attach:\" line found in stdout: {stdout:?}"))
        .strip_prefix("attach: ")
        .unwrap();

    let argv = shlex::split(attach_line)
        .unwrap_or_else(|| panic!("attach line is not valid shell input at all: {attach_line:?}"));
    assert_eq!(
        argv,
        vec![
            "warden-tui".to_string(),
            "attach".to_string(),
            "--run-id".to_string(),
            run_id.to_string(),
            "--warden-home".to_string(),
            warden_home.to_str().unwrap().to_string(),
        ],
        "shell-splitting the attach line must recover the space-containing \
         warden_home as a single argv entry, not stray extra tokens: \
         {attach_line:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_attach_command_absolutizes_a_relative_warden_home() {
    let repo = init_test_repo();
    let cwd_root = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    let relative_warden_home = "relative_warden_home";
    let output = warden_command()
        .0
        .current_dir(cwd_root.path())
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", cwd_root.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "absolutize a relative warden-home",
            "--warden-home",
            relative_warden_home,
            "--tool",
            "claude",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        !stdout.contains(&format!("--warden-home {relative_warden_home}")),
        "the attach command must not echo the raw relative --warden-home value verbatim: \
         {stdout:?}"
    );

    let expected_absolute = cwd_root
        .path()
        .canonicalize()
        .expect("cwd_root exists")
        .join(relative_warden_home);
    let expected_fragment = format!("--warden-home {}", expected_absolute.display());
    assert!(
        stdout.contains(&expected_fragment),
        "expected the attach command to name the absolutized relative warden-home \
         ({expected_fragment:?}), got stdout: {stdout:?}"
    );

    assert!(
        expected_absolute.join("state.db").exists(),
        "the absolutized relative warden-home ({}) must be where this run's state.db actually \
         is",
        expected_absolute.display()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_run_survives_a_closed_stdout_without_panicking() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    let bin_path = env!("CARGO_BIN_EXE_warden");
    let mut child = SyncCommand::new(bin_path)
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "survive a closed stdout",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn warden");

    let child_stdout = child.stdout.take().expect("piped stdout");
    let child_stderr = child.stderr.take().expect("piped stderr");

    let stderr_thread = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let mut stderr = child_stderr;
        stderr.read_to_string(&mut buf).ok();
        buf
    });

    let first_line = {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(child_stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("read the \"started\" line before closing stdout");
        line
    };

    let run_id = first_line
        .trim_end()
        .strip_prefix("run ")
        .and_then(|rest| rest.strip_suffix(" started"))
        .unwrap_or_else(|| panic!("unexpected first stdout line: {first_line:?}"))
        .to_string();

    let status = child.wait().expect("wait for warden to exit");
    let stderr_output = stderr_thread.join().expect("stderr thread");

    assert!(
        status.success(),
        "warden run must still exit successfully despite its stdout being closed mid-run \
         (status: {status:?}, stderr: {stderr_output:?})"
    );
    assert!(
        !stderr_output.contains("panicked"),
        "warden run must not panic when stdout is closed mid-run; stderr: {stderr_output:?}"
    );

    let pool = warden::db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let run = warden::db::get_run(&pool, &run_id)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no `runs` row found for id {run_id}"));
    assert_eq!(
        run.state,
        RunState::Converged,
        "a closed stdout must not leave the run's own SQLite state stuck non-terminal"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_full_convergence_cycle_reboucles_then_converges_via_cli() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        FLIP_STATUS_CODER_BODY,
        STATUS_GATED_REVIEWER_BODY,
        NOOP_BODY,
    );

    let before_status = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(before_status.stdout.is_empty(), "repo must start clean");

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "flip status to fixed",
            "--branch",
            "main",
            "--max-review-cycles",
            "5",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let after_status = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        after_status.stdout.is_empty(),
        "main repo working tree must be untouched by the run: {:?}",
        String::from_utf8_lossy(&after_status.stdout)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_zero_md_run_uses_the_adapters_defaults_and_converges() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "add a note",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_run_via_codex_converges_and_extracts_findings_and_usage() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let reviewer_body = r#"
printf '%s\n' '{"source":"reviewer","severity":"info","description":"codex reviewer saw the diff"}' > "$WARDEN_RESULT_FILE"
"#;
    write_fake_codex(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        reviewer_body,
        NOOP_BODY,
    );

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "add a note via codex",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "codex",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();

    let (cycle_id,): (String,) = sqlx::query_as("SELECT id FROM cycles WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let findings = warden::db::list_findings_for_cycle(&pool, &cycle_id)
        .await
        .unwrap();
    assert_eq!(findings.len(), 1, "expected the reviewer's one finding");
    assert_eq!(findings[0].source, FindingSource::role("reviewer"));
    assert_eq!(findings[0].description, "codex reviewer saw the diff");

    let usage = warden::db::get_run_token_usage(&pool, &run_id)
        .await
        .unwrap()
        .expect("codex's modeled token_count events must be extracted, not \"n/a\"");
    assert!(usage.input_tokens > 0);
    assert!(usage.output_tokens > 0);
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_run_via_mistral_converges_and_extracts_findings() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let tester_body = r#"
printf '%s\n' '{"source":"tester","severity":"info","description":"mistral tester ran the suite"}' > "$WARDEN_RESULT_FILE"
"#;
    write_fake_mistral(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        tester_body,
    );

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "add a note via mistral",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "mistral",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();

    let (cycle_id,): (String,) = sqlx::query_as("SELECT id FROM cycles WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let findings = warden::db::list_findings_for_cycle(&pool, &cycle_id)
        .await
        .unwrap();
    assert_eq!(findings.len(), 1, "expected the tester's one finding");
    assert_eq!(findings[0].source, FindingSource::role("tester"));
    assert_eq!(findings[0].description, "mistral tester ran the suite");

    assert_eq!(
        warden::db::get_run_token_usage(&pool, &run_id)
            .await
            .unwrap(),
        None,
        "MistralAdapter never reports usage -- must persist as \"n/a\", not a fabricated figure"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_failing_coder_marks_run_failed_and_never_reaches_review() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        "exit 1",
        "printf 'unreachable' > /dev/stderr; exit 1",
        "printf 'unreachable' > /dev/stderr; exit 1",
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "this will fail",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure();
}

#[test]
fn e2e_blank_intent_is_a_clean_cli_error_and_creates_no_run_row() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.args([
        "run",
        "--repo",
        repo.path().to_str().unwrap(),
        "--intent",
        "",
        "--warden-home",
        warden_home.path().to_str().unwrap(),
        "--tool",
        "claude",
    ])
    .assert()
    .failure()
    .stderr(contains("must not be blank"));

    assert!(
        !warden_home.path().join("state.db").exists(),
        "a rejected --intent must never reach the point of creating the state db"
    );
}

#[test]
fn e2e_whitespace_only_intent_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.args([
        "run",
        "--repo",
        repo.path().to_str().unwrap(),
        "--intent",
        "   \n\t  ",
        "--warden-home",
        warden_home.path().to_str().unwrap(),
        "--tool",
        "claude",
    ])
    .assert()
    .failure()
    .stderr(contains("must not be blank"));
}

#[test]
fn e2e_non_git_repo_path_is_a_clean_cli_error() {
    let not_a_repo = TempDir::new().unwrap();
    let warden_home = TempDir::new().unwrap();
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.args([
        "run",
        "--repo",
        not_a_repo.path().to_str().unwrap(),
        "--intent",
        "irrelevant",
        "--warden-home",
        warden_home.path().to_str().unwrap(),
        "--tool",
        "claude",
    ])
    .assert()
    .failure();
}

#[test]
fn e2e_an_unknown_tool_is_a_clean_cli_error_naming_the_value() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.args([
        "run",
        "--repo",
        repo.path().to_str().unwrap(),
        "--intent",
        "irrelevant",
        "--warden-home",
        warden_home.path().to_str().unwrap(),
        "--tool",
        "aider",
    ])
    .assert()
    .failure()
    .stderr(contains("aider"));
}

#[test]
fn e2e_omitting_tool_entirely_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.args([
        "run",
        "--repo",
        repo.path().to_str().unwrap(),
        "--intent",
        "irrelevant",
        "--warden-home",
        warden_home.path().to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(contains("--tool"));
}

#[test]
fn e2e_an_unknown_isolation_is_a_clean_cli_error_naming_the_value() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.args([
        "run",
        "--repo",
        repo.path().to_str().unwrap(),
        "--intent",
        "irrelevant",
        "--warden-home",
        warden_home.path().to_str().unwrap(),
        "--tool",
        "claude",
        "--isolation",
        "firecracker",
    ])
    .assert()
    .failure()
    .stderr(contains("firecracker"));
}

#[test]
fn e2e_omitting_isolation_entirely_defaults_to_worktree_not_a_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.env("PATH", "/usr/bin:/bin")
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "irrelevant",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stderr(contains("ADR-0021"));
}

#[test]
fn e2e_isolation_docker_never_prints_the_worktree_filesystem_warning() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.args([
        "run",
        "--repo",
        repo.path().to_str().unwrap(),
        "--intent",
        "irrelevant",
        "--warden-home",
        warden_home.path().to_str().unwrap(),
        "--tool",
        "claude",
        "--isolation",
        "docker",
    ])
    .assert()
    .failure()
    .stderr(contains("ADR-0021").not());
}

#[test]
fn e2e_isolation_worktree_warning_survives_rust_log_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.env("PATH", "/usr/bin:/bin")
        .env("RUST_LOG", "error")
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "irrelevant",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stderr(contains("ADR-0021"));
}

#[cfg(unix)]
fn docker_daemon_available() -> bool {
    SyncCommand::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn docker_host_for_current_context() -> Option<String> {
    if let Ok(explicit) = std::env::var("DOCKER_HOST") {
        if !explicit.is_empty() {
            return Some(explicit);
        }
    }
    let output = SyncCommand::new("docker")
        .args([
            "context",
            "inspect",
            "--format",
            "{{.Endpoints.docker.Host}}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let host = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

#[cfg(unix)]
#[test]
fn e2e_isolation_docker_actually_runs_the_coder_inside_a_real_container() {
    if !docker_daemon_available() {
        eprintln!("skipping: no docker daemon reachable");
        return;
    }

    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let (mut cmd, hermetic_home) = warden_command();
    if let Some(docker_host) = docker_host_for_current_context() {
        cmd.env("DOCKER_HOST", docker_host);
    }

    let claude_dir = hermetic_home.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join(".credentials.json"), "{}").unwrap();

    let image_tag = format!("warden-cli-test-{}", uuid::Uuid::new_v4());
    let build_dir = TempDir::new().unwrap();
    std::fs::write(
        build_dir.path().join("Dockerfile"),
        "FROM alpine:3.24.1@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b\n\
         RUN apk add --no-cache git\n\
         COPY claude /usr/local/bin/claude\n\
         RUN chmod +x /usr/local/bin/claude\n",
    )
    .unwrap();
    std::fs::write(
        build_dir.path().join("claude"),
        r#"#!/bin/sh
set -e
stdin_content=$(cat)

if [ ! -f /.dockerenv ]; then
    echo "fake claude: not running inside a container" >&2
    exit 1
fi

if echo "$stdin_content" | grep -q '"role":"coder"'; then
    git_common_dir=$(git rev-parse --git-common-dir)
    hostname > "$git_common_dir/WARDEN_DOCKER_ISOLATION_PROOF"
    echo containerized > proof.txt
    git add proof.txt
    git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle (containerized)"
fi

printf '{"type":"result","subtype":"success","is_error":false,"result":""}\n'
"#,
    )
    .unwrap();

    let build_status = SyncCommand::new("docker")
        .args(["build", "-q", "-t", &image_tag])
        .arg(build_dir.path())
        .status()
        .expect("spawn docker build");
    assert!(
        build_status.success(),
        "failed to build the throwaway test image {image_tag}"
    );

    let assert = cmd
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "run the coder inside a real docker container",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
            "--isolation",
            "docker",
            "--isolation-image",
            &image_tag,
        ])
        .assert();

    let output = assert.get_output().clone();
    let _ = SyncCommand::new("docker")
        .args(["rmi", "-f", &image_tag])
        .status();

    assert!(
        output.status.success(),
        "warden run --isolation docker did not exit successfully: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("finished: Converged"),
        "expected a converged run, got stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let proof_path = repo
        .path()
        .join(".git")
        .join("WARDEN_DOCKER_ISOLATION_PROOF");
    let container_hostname = std::fs::read_to_string(&proof_path).unwrap_or_else(|error| {
        panic!(
            "expected {} (proof the coder ran inside a container) to exist after a converged \
             --isolation docker run: {error}",
            proof_path.display()
        )
    });
    let host_hostname = SyncCommand::new("hostname")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    assert_ne!(
        container_hostname.trim(),
        host_hostname.trim(),
        "the proof file's hostname must be the *container's* own (Docker sets it to the \
         container id by default), not this host's -- got {container_hostname:?} on a host \
         named {host_hostname:?}"
    );
}

#[test]
fn e2e_the_removed_agent_flags_are_rejected_by_the_cli_not_silently_ignored() {
    for removed_flag in ["--coder-agent", "--reviewer-agent", "--tester-agent"] {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let (mut cmd, _hermetic_home) = warden_command();

        cmd.args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "irrelevant",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
            removed_flag,
            "/dev/null",
        ])
        .assert()
        .failure()
        .stderr(contains("unexpected argument"));

        assert!(
            !warden_home.path().join("state.db").exists(),
            "{removed_flag} must be rejected during arg parsing, before any state db is created"
        );
    }
}

#[test]
fn e2e_an_agent_definition_with_an_unknown_key_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    write_agent_definition(
        repo.path(),
        "coder",
        "model: opus\ntimeout: 30\n",
        "be a coder",
    );
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.args([
        "run",
        "--repo",
        repo.path().to_str().unwrap(),
        "--intent",
        "irrelevant",
        "--warden-home",
        warden_home.path().to_str().unwrap(),
        "--tool",
        "claude",
    ])
    .assert()
    .failure()
    .stderr(contains("timeout"));
}

#[test]
fn e2e_an_agent_definition_with_a_blank_system_prompt_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    write_agent_definition(repo.path(), "coder", "", "   ");
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.args([
        "run",
        "--repo",
        repo.path().to_str().unwrap(),
        "--intent",
        "irrelevant",
        "--warden-home",
        warden_home.path().to_str().unwrap(),
        "--tool",
        "claude",
    ])
    .assert()
    .failure()
    .stderr(contains("blank"));
}

#[test]
fn e2e_a_crlf_definition_file_is_rejected_naming_the_line_endings_not_the_fence() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let agents_dir = repo.path().join(".warden/agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("coder.md"),
        "---\r\nmodel: opus\r\n---\r\nbe a coder\r\n",
    )
    .unwrap();
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.args([
        "run",
        "--repo",
        repo.path().to_str().unwrap(),
        "--intent",
        "irrelevant",
        "--warden-home",
        warden_home.path().to_str().unwrap(),
        "--tool",
        "claude",
    ])
    .assert()
    .failure()
    .stderr(contains("CRLF"));
}

#[test]
fn e2e_a_definition_path_that_is_a_directory_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    std::fs::create_dir_all(repo.path().join(".warden/agents/coder.md")).unwrap();
    let (mut cmd, _hermetic_home) = warden_command();

    cmd.args([
        "run",
        "--repo",
        repo.path().to_str().unwrap(),
        "--intent",
        "irrelevant",
        "--warden-home",
        warden_home.path().to_str().unwrap(),
        "--tool",
        "claude",
    ])
    .assert()
    .failure();
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_reviewer_and_tester_receive_target_commit_diff_and_role_on_stdin() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        &format!(
            r#"cp "$stdin_file" "{captures}/reviewer_stdin.json""#,
            captures = captures.path().display()
        ),
        &format!(
            r#"cp "$stdin_file" "{captures}/tester_stdin.json""#,
            captures = captures.path().display()
        ),
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "add a note",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    for role in ["reviewer", "tester"] {
        let raw =
            std::fs::read_to_string(captures.path().join(format!("{role}_stdin.json"))).unwrap();
        let payload = warden_core::parse_agent_input_message(&raw)
            .expect("a payload warden's own parser accepts");
        assert_eq!(payload.role.as_str(), role);
        assert!(payload.target_commit.is_some());
        assert!(payload.diff.as_deref().unwrap().contains("notes.txt"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_coder_receives_the_run_intent_on_stdin_as_a_versioned_role_tagged_payload() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();
    write_agent_definition(repo.path(), "coder", "", "be a coder");

    write_fake_claude(
        bin_dir.path(),
        &format!(
            "cp \"$stdin_file\" \"{captures}/coder_stdin.json\"\n{APPEND_NOTES_CODER_BODY}",
            captures = captures.path().display()
        ),
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "add a note please",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let raw = std::fs::read_to_string(captures.path().join("coder_stdin.json")).unwrap();
    let payload = warden_core::parse_agent_input_message(&raw).unwrap();
    assert_eq!(payload.role, warden_core::AgentRole::Coder);
    assert_eq!(payload.intent.as_deref(), Some("add a note please"));
    assert_eq!(payload.system_prompt.trim(), "be a coder");
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_home_reaches_claude_but_other_env_vars_do_not() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    write_fake_claude(
        bin_dir.path(),
        &format!(
            "env | sort > \"{captures}/coder_env.txt\"\n{APPEND_NOTES_CODER_BODY}",
            captures = captures.path().display()
        ),
        NOOP_BODY,
        NOOP_BODY,
    );

    let marker_value = "WARDEN_E2E_ENV_LEAK_MARKER_71a2";
    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .env("WARDEN_TEST_SECRET", marker_value)
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "add a note",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let env_dump = std::fs::read_to_string(captures.path().join("coder_env.txt")).unwrap();
    assert!(
        env_dump.contains("HOME="),
        "HOME must reach claude (ClaudeAdapter::env_allowlist): {env_dump:?}"
    );
    assert!(
        env_dump.contains("PATH="),
        "PATH must still reach claude: {env_dump:?}"
    );
    assert!(
        !env_dump.contains(marker_value),
        "an arbitrary orchestrator env var must never leak into the agent's environment: \
         {env_dump:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_run_intent_never_leaks_into_argv() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    write_fake_claude(
        bin_dir.path(),
        &format!(
            "printf '%s' \"$0 $*\" > \"{captures}/coder_argv.txt\"\ncp \"$stdin_file\" \"{captures}/coder_stdin.json\"\n{APPEND_NOTES_CODER_BODY}",
            captures = captures.path().display()
        ),
        NOOP_BODY,
        NOOP_BODY,
    );

    let marker = "WARDEN_SECRET_INTENT_MARKER_9f3d21";
    let intent = format!("do the thing ({marker})");

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            &intent,
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let argv_dump = std::fs::read_to_string(captures.path().join("coder_argv.txt")).unwrap();
    assert!(
        !argv_dump.contains(marker),
        "the run intent must never leak into the coder's argv: {argv_dump:?}"
    );

    let stdin_dump = std::fs::read_to_string(captures.path().join("coder_stdin.json")).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&stdin_dump).unwrap();
    assert_eq!(payload["intent"], intent);
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_reviewer_findings_extracted_through_the_claude_json_envelope_reach_max_review_cycles()
{
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        r#"printf '%s\n' '{"source":"reviewer","severity":"blocking","description":"always blocking"}' > "$WARDEN_RESULT_FILE""#,
        NOOP_BODY,
    );

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "never converges",
            "--max-review-cycles",
            "2",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: StepCyclesExceeded(1)"));

    let run_id = extract_run_id(&String::from_utf8_lossy(&assert.get_output().stdout));
    let pool = warden::db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::StepCyclesExceeded(1));
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_mistral_reviewer_exiting_nonzero_with_no_output_never_converges() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_mistral(bin_dir.path(), APPEND_NOTES_CODER_BODY, "exit 1", NOOP_BODY);

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "reviewer crashes silently, must never converge",
            "--max-review-cycles",
            "2",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "mistral",
        ])
        .assert()
        .success()
        .stdout(contains("finished: StepCyclesExceeded(1)"));

    let run_id = extract_run_id(&String::from_utf8_lossy(&assert.get_output().stdout));
    let pool = warden::db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::StepCyclesExceeded(1));

    let (cycle_id,): (String,) = sqlx::query_as("SELECT id FROM cycles WHERE run_id = ? LIMIT 1")
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let findings = warden::db::list_findings_for_cycle(&pool, &cycle_id)
        .await
        .unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f.source == FindingSource::role("reviewer")
                && f.severity == warden_core::Severity::Blocking
                && f.description.contains("exited with status")),
        "expected a synthesized non-zero-exit blocking finding, got: {findings:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_definition_model_and_tools_reach_the_claude_invocation_argv() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    write_agent_definition(
        repo.path(),
        "coder",
        "model: opus\ntools: Read, Edit, Bash\n",
        "be a coder",
    );
    write_fake_claude(
        bin_dir.path(),
        &format!(
            "printf '%s' \"$*\" > \"{captures}/coder_argv.txt\"\n{APPEND_NOTES_CODER_BODY}",
            captures = captures.path().display()
        ),
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "add a note",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let argv_dump = std::fs::read_to_string(captures.path().join("coder_argv.txt")).unwrap();
    assert!(argv_dump.contains("--model opus"), "{argv_dump:?}");
    assert!(
        argv_dump.contains("--allowedTools Read, Edit, Bash"),
        "{argv_dump:?}"
    );
    assert!(
        argv_dump.contains("--append-system-prompt be a coder"),
        "{argv_dump:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_reviewer_and_tester_definitions_each_reach_their_own_invocation_not_each_others() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();
    let user_config = TempDir::new().unwrap();

    write_agent_definition(repo.path(), "coder", "", "be the coder");
    write_user_config_agent_definition(
        user_config.path(),
        "reviewer",
        "model: haiku\ntools: Read, Grep\n",
        "be the reviewer",
    );
    write_user_config_agent_definition(
        user_config.path(),
        "tester",
        "model: opus\ntools: Read, Bash\n",
        "be the tester",
    );

    write_fake_claude(
        bin_dir.path(),
        &format!(
            "printf '%s' \"$*\" > \"{captures}/coder_argv.txt\"\n{APPEND_NOTES_CODER_BODY}",
            captures = captures.path().display()
        ),
        &format!(
            "printf '%s' \"$*\" > \"{captures}/reviewer_argv.txt\"",
            captures = captures.path().display()
        ),
        &format!(
            "printf '%s' \"$*\" > \"{captures}/tester_argv.txt\"",
            captures = captures.path().display()
        ),
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", user_config.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "add a note",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let coder_argv = std::fs::read_to_string(captures.path().join("coder_argv.txt")).unwrap();
    let reviewer_argv = std::fs::read_to_string(captures.path().join("reviewer_argv.txt")).unwrap();
    let tester_argv = std::fs::read_to_string(captures.path().join("tester_argv.txt")).unwrap();

    assert!(
        coder_argv.contains("--append-system-prompt be the coder"),
        "{coder_argv:?}"
    );
    assert!(!coder_argv.contains("--model"), "{coder_argv:?}");

    assert!(
        reviewer_argv.contains("--append-system-prompt be the reviewer"),
        "{reviewer_argv:?}"
    );
    assert!(reviewer_argv.contains("--model haiku"), "{reviewer_argv:?}");
    assert!(
        reviewer_argv.contains("--allowedTools Read, Grep"),
        "{reviewer_argv:?}"
    );
    assert!(
        !reviewer_argv.contains("be the tester"),
        "the reviewer's argv must never carry the tester's own prompt: {reviewer_argv:?}"
    );

    assert!(
        tester_argv.contains("--append-system-prompt be the tester"),
        "{tester_argv:?}"
    );
    assert!(tester_argv.contains("--model opus"), "{tester_argv:?}");
    assert!(
        tester_argv.contains("--allowedTools Read, Bash"),
        "{tester_argv:?}"
    );
    assert!(
        !tester_argv.contains("be the reviewer"),
        "the tester's argv must never carry the reviewer's own prompt: {tester_argv:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_repo_reviewer_definition_is_ignored_by_default_and_warns() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    write_agent_definition(
        repo.path(),
        "reviewer",
        "",
        "REPO_CONTROLLED_REVIEWER_MARKER_PROMPT",
    );
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        &format!(
            r#"cp "$stdin_file" "{captures}/reviewer_stdin.json""#,
            captures = captures.path().display()
        ),
        NOOP_BODY,
    );

    let (mut cmd, _hermetic_home) = warden_command();
    let assert = cmd
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "trust-repo-agents off: repo reviewer.md must be ignored",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let expected_path = repo.path().join(".warden/agents/reviewer.md");
    assert!(
        combined.contains("ignoring a repo-controlled agent definition"),
        "{combined:?}"
    );
    assert!(
        combined.contains(&expected_path.display().to_string()),
        "{combined:?}"
    );

    let raw = std::fs::read_to_string(captures.path().join("reviewer_stdin.json")).unwrap();
    let payload = warden_core::parse_agent_input_message(&raw).unwrap();
    assert!(
        !payload
            .system_prompt
            .contains("REPO_CONTROLLED_REVIEWER_MARKER_PROMPT"),
        "the ignored repo definition must never reach the reviewer's own invocation: {}",
        payload.system_prompt
    );
    assert!(
        payload.system_prompt.contains("Warden's reviewer agent"),
        "the adapter's own default prompt must be used instead: {}",
        payload.system_prompt
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_trust_repo_agents_uses_the_repo_definition_and_surfaces_it_as_untrusted() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    write_agent_definition(
        repo.path(),
        "reviewer",
        "",
        "REPO_CONTROLLED_REVIEWER_MARKER_PROMPT",
    );
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        &format!(
            r#"cp "$stdin_file" "{captures}/reviewer_stdin.json""#,
            captures = captures.path().display()
        ),
        NOOP_BODY,
    );

    let (mut cmd, _hermetic_home) = warden_command();
    let assert = cmd
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "trust-repo-agents on: repo reviewer.md must be used, surfaced as untrusted",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
            "--trust-repo-agents",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let expected_path = repo.path().join(".warden/agents/reviewer.md");
    assert!(combined.contains("NOT trusted"), "{combined:?}");
    assert!(
        combined.contains(&expected_path.display().to_string()),
        "{combined:?}"
    );

    let raw = std::fs::read_to_string(captures.path().join("reviewer_stdin.json")).unwrap();
    let payload = warden_core::parse_agent_input_message(&raw).unwrap();
    assert!(
        payload
            .system_prompt
            .contains("REPO_CONTROLLED_REVIEWER_MARKER_PROMPT"),
        "the repo's own definition must reach the reviewer's invocation once trusted: {}",
        payload.system_prompt
    );

    let run_id = extract_run_id(&String::from_utf8_lossy(&assert.get_output().stdout));
    let pool = warden::db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let events = warden::db::list_events_for_run(&pool, &run_id)
        .await
        .unwrap();
    let expected_canonical_path = expected_path.canonicalize().unwrap();
    assert!(
        events.iter().any(|entry| matches!(
            entry.event(),
            Some(warden_core::RunEvent::UntrustedAgentDefinitionUsed { role, path, canonical_path })
                if role == "reviewer"
                    && path == &expected_path.display().to_string()
                    && canonical_path == &expected_canonical_path.display().to_string()
        )),
        "expected an UntrustedAgentDefinitionUsed event for the reviewer naming {}: {events:?}",
        expected_path.display()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_xdg_config_home_pointing_inside_the_repo_is_degraded_to_untrusted() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();
    let malicious_xdg_config_home = repo.path().join(".config");
    write_user_config_agent_definition(
        &malicious_xdg_config_home,
        "reviewer",
        "",
        "REPO_CONTROLLED_VIA_XDG_MARKER_PROMPT",
    );
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        &format!(
            r#"cp "$stdin_file" "{captures}/reviewer_stdin.json""#,
            captures = captures.path().display()
        ),
        NOOP_BODY,
    );
    let expected_path = malicious_xdg_config_home
        .join("warden")
        .join("agents")
        .join("reviewer.md");

    {
        let (mut cmd, _hermetic_home) = warden_command();
        let assert = cmd
            .env("PATH", path_with_fake_bin_first(bin_dir.path()))
            .env("XDG_CONFIG_HOME", &malicious_xdg_config_home)
            .args([
                "run",
                "--repo",
                repo.path().to_str().unwrap(),
                "--intent",
                "HIGH fix, flag off: XDG-inside-repo must be ignored",
                "--warden-home",
                warden_home.path().to_str().unwrap(),
                "--tool",
                "claude",
            ])
            .assert()
            .success()
            .stdout(contains("finished: Converged"));

        let output = assert.get_output();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            combined.contains(
                "ignoring a reviewer/tester definition that looked like the trusted user \
                 config source"
            ),
            "{combined:?}"
        );
        assert!(
            !combined.contains("move it to $XDG_CONFIG_HOME/warden/agents/"),
            "the degraded-user-config case must not get the plain repo-convention advice, \
             which is a no-op for a file already at that exact location: {combined:?}"
        );
        assert!(
            combined.contains(&expected_path.display().to_string()),
            "{combined:?}"
        );

        let raw = std::fs::read_to_string(captures.path().join("reviewer_stdin.json")).unwrap();
        let payload = warden_core::parse_agent_input_message(&raw).unwrap();
        assert!(
            !payload
                .system_prompt
                .contains("REPO_CONTROLLED_VIA_XDG_MARKER_PROMPT"),
            "{}",
            payload.system_prompt
        );
    }

    {
        let (mut cmd, _hermetic_home) = warden_command();
        let assert = cmd
            .env("PATH", path_with_fake_bin_first(bin_dir.path()))
            .env("XDG_CONFIG_HOME", &malicious_xdg_config_home)
            .args([
                "run",
                "--repo",
                repo.path().to_str().unwrap(),
                "--intent",
                "HIGH fix, flag on: XDG-inside-repo must be used, surfaced as untrusted",
                "--warden-home",
                warden_home.path().to_str().unwrap(),
                "--tool",
                "claude",
                "--trust-repo-agents",
            ])
            .assert()
            .success()
            .stdout(contains("finished: Converged"));

        let output = assert.get_output();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(combined.contains("NOT trusted"), "{combined:?}");
        assert!(
            combined.contains(&expected_path.display().to_string()),
            "{combined:?}"
        );

        let raw = std::fs::read_to_string(captures.path().join("reviewer_stdin.json")).unwrap();
        let payload = warden_core::parse_agent_input_message(&raw).unwrap();
        assert!(
            payload
                .system_prompt
                .contains("REPO_CONTROLLED_VIA_XDG_MARKER_PROMPT"),
            "{}",
            payload.system_prompt
        );

        let run_id = extract_run_id(&String::from_utf8_lossy(&assert.get_output().stdout));
        let pool = warden::db::connect(&warden_home.path().join("state.db"))
            .await
            .unwrap();
        let events = warden::db::list_events_for_run(&pool, &run_id)
            .await
            .unwrap();
        let expected_canonical_path = expected_path.canonicalize().unwrap();
        assert!(
            events.iter().any(|entry| matches!(
                entry.event(),
                Some(warden_core::RunEvent::UntrustedAgentDefinitionUsed { role, path, canonical_path })
                    if role == "reviewer"
                        && path == &expected_path.display().to_string()
                        && canonical_path == &expected_canonical_path.display().to_string()
            )),
            "{events:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_user_config_dir_falls_back_to_home_dot_config_when_xdg_is_unset() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();
    let fake_home = TempDir::new().unwrap();

    write_user_config_agent_definition(
        &fake_home.path().join(".config"),
        "reviewer",
        "",
        "HOME_FALLBACK_REVIEWER_MARKER_PROMPT",
    );
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        &format!(
            r#"cp "$stdin_file" "{captures}/reviewer_stdin.json""#,
            captures = captures.path().display()
        ),
        NOOP_BODY,
    );

    warden_command()
        .0
        .env_remove("XDG_CONFIG_HOME")
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("HOME", fake_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "HOME fallback when XDG_CONFIG_HOME is unset",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let raw = std::fs::read_to_string(captures.path().join("reviewer_stdin.json")).unwrap();
    let payload = warden_core::parse_agent_input_message(&raw).unwrap();
    assert!(
        payload
            .system_prompt
            .contains("HOME_FALLBACK_REVIEWER_MARKER_PROMPT"),
        "expected the $HOME/.config fallback to resolve the reviewer's trusted definition: {}",
        payload.system_prompt
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_user_config_dir_falls_back_to_home_dot_config_when_xdg_is_blank() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();
    let fake_home = TempDir::new().unwrap();

    write_user_config_agent_definition(
        &fake_home.path().join(".config"),
        "reviewer",
        "",
        "HOME_FALLBACK_REVIEWER_MARKER_PROMPT",
    );
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        &format!(
            r#"cp "$stdin_file" "{captures}/reviewer_stdin.json""#,
            captures = captures.path().display()
        ),
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", "   ")
        .env("HOME", fake_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "HOME fallback when XDG_CONFIG_HOME is blank",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let raw = std::fs::read_to_string(captures.path().join("reviewer_stdin.json")).unwrap();
    let payload = warden_core::parse_agent_input_message(&raw).unwrap();
    assert!(
        payload
            .system_prompt
            .contains("HOME_FALLBACK_REVIEWER_MARKER_PROMPT"),
        "expected a blank XDG_CONFIG_HOME to fall back to $HOME/.config: {}",
        payload.system_prompt
    );
}

#[test]
fn e2e_missing_xdg_config_home_and_home_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();

    warden_command()
        .0
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME")
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "irrelevant",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stderr(contains("cannot resolve the user config directory"));
}

#[test]
fn e2e_empty_home_with_no_xdg_config_home_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();

    warden_command()
        .0
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", "")
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "irrelevant",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stderr(contains("cannot resolve the user config directory"));
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_a_workflow_with_no_reviewer_or_tester_step_never_needs_home_or_xdg_config_home() {
    let repo = init_test_repo();
    write_workflow_yaml(
        repo.path(),
        r#"
name: coder-only
steps:
  - role: coder
    agent: coder
"#,
    );

    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME")
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "no reviewer/tester step at all",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"))
        .stderr(contains("cannot resolve the user config directory").not());
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_a_workflow_with_a_custom_role_but_no_reviewer_or_tester_never_needs_home_or_xdg_config_home(
) {
    let repo = init_test_repo();
    write_workflow_yaml(
        repo.path(),
        r#"
name: coder-plus-custom
steps:
  - role: coder
    agent: coder
  - role: techlead
    agent: techlead
"#,
    );
    write_custom_step_agent_definition(
        repo.path(),
        "techlead",
        "You are Warden's tech lead, running with no reviewer/tester in the pipeline.",
    );

    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME")
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "custom role, no reviewer/tester step",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"))
        .stderr(contains("cannot resolve the user config directory").not());
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_a_coder_only_workflow_never_resolves_user_config_dir_even_when_the_coder_itself_cannot_spawn(
) {
    let repo = init_test_repo();
    write_workflow_yaml(
        repo.path(),
        r#"
name: coder-only
steps:
  - role: coder
    agent: coder
"#,
    );

    let warden_home = TempDir::new().unwrap();
    let empty_bin_dir = TempDir::new().unwrap();

    warden_command()
        .0
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME")
        .env("PATH", empty_bin_dir.path().to_str().unwrap())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "coder-only workflow, no claude binary on PATH at all",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stderr(contains("No such file or directory"))
        .stderr(contains("cannot resolve the user config directory").not());
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_user_reaches_claude_alongside_home() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    write_fake_claude(
        bin_dir.path(),
        &format!(
            "env | sort > \"{captures}/coder_env.txt\"\n{APPEND_NOTES_CODER_BODY}",
            captures = captures.path().display()
        ),
        NOOP_BODY,
        NOOP_BODY,
    );

    let expected_user = std::env::var("USER").unwrap_or_default();

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "add a note",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let env_dump = std::fs::read_to_string(captures.path().join("coder_env.txt")).unwrap();
    if expected_user.is_empty() {
        eprintln!(
            "USER is unset in this test environment; skipping the positive assertion, \
             spawn_with_extra_env only forwards variables actually set in warden's own env"
        );
    } else {
        assert!(
            env_dump.contains(&format!("USER={expected_user}")),
            "USER must reach claude (ClaudeAdapter::env_allowlist): {env_dump:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_crashed_run_is_marked_failed_on_the_next_cli_invocation() {
    let warden_home = TempDir::new().unwrap();
    let db_path = warden_home.path().join("state.db");

    {
        let pool = warden::db::connect(&db_path).await.unwrap();
        warden::db::insert_run(
            &pool,
            "crashed-run",
            "/tmp/some-repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        warden::db::update_run_state(&pool, "crashed-run", RunState::CoderRunning)
            .await
            .unwrap();
        warden::db::insert_cycle(&pool, "crashed-cycle", "crashed-run", 1)
            .await
            .unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id().unwrap();
        child.wait().await.unwrap();

        warden::db::insert_agent_process(
            &pool,
            "crashed-process",
            "crashed-cycle",
            "coder",
            dead_pid,
            "/tmp/wt",
        )
        .await
        .unwrap();
        pool.close().await;
    }

    let repo = init_test_repo();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "unrelated new run",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let pool = warden::db::connect(&db_path).await.unwrap();
    let recovered = warden::db::get_run(&pool, "crashed-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered.state,
        RunState::Failed,
        "a run left mid-cycle with no live process must be marked Failed on the next CLI startup"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_converged_commit_is_persisted_and_protected_without_touching_main_branch() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    let original_head_ref = String::from_utf8_lossy(
        &SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["symbolic-ref", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    let original_commit_sha = String::from_utf8_lossy(
        &SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "single converging cycle",
            "--branch",
            "main",
            "--max-review-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let after_head_ref = String::from_utf8_lossy(
        &SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["symbolic-ref", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(after_head_ref, original_head_ref);
    let after_commit_sha = String::from_utf8_lossy(
        &SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(
        after_commit_sha, original_commit_sha,
        "main repo's checked-out commit must be unchanged by `warden run`"
    );
    let status = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty(),
        "main repo working tree must stay clean"
    );

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    let converged_sha = run
        .converged_commit_sha
        .expect("a Converged run must have a persisted converged_commit_sha");
    assert_eq!(
        converged_sha.len(),
        40,
        "expected a full SHA-1 hex commit id"
    );
    assert_ne!(
        converged_sha, original_commit_sha,
        "converged commit must be the coder's new commit, not the repo's original HEAD"
    );

    let (cycle_sha,): (Option<String>,) =
        sqlx::query_as("SELECT coder_commit_sha FROM cycles WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        cycle_sha.as_deref(),
        Some(converged_sha.as_str()),
        "cycles.coder_commit_sha must match the run's converged_commit_sha for a single-cycle run"
    );

    let ref_name = format!("refs/warden/runs/{run_id}/cycle-1");
    let ref_lookup = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["rev-parse", &ref_name])
        .output()
        .unwrap();
    assert!(
        ref_lookup.status.success(),
        "expected protective ref {ref_name} to exist in the main repo"
    );
    assert_eq!(
        String::from_utf8_lossy(&ref_lookup.stdout).trim(),
        converged_sha,
        "the protective ref must point at the same commit persisted in SQLite"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_reviewer_and_tester_modify_different_files_without_collision() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();

    let coder_body = r#"
echo original-review > review_target.txt
echo original-test > test_target.txt
git add review_target.txt test_target.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#;
    let reviewer_body = r#"
echo modified-by-reviewer > review_target.txt
seen=$(cat test_target.txt)
printf '{"source":"reviewer","severity":"info","description":"review_target=modified-by-reviewer test_target_seen=%s"}\n' "$seen" > "$WARDEN_RESULT_FILE"
"#;
    let tester_body = r#"
echo modified-by-tester > test_target.txt
seen=$(cat review_target.txt)
printf '{"source":"tester","severity":"info","description":"test_target=modified-by-tester review_target_seen=%s"}\n' "$seen" > "$WARDEN_RESULT_FILE"
"#;
    write_fake_claude(bin_dir.path(), coder_body, reviewer_body, tester_body);

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "crossed findings, no collision",
            "--branch",
            "main",
            "--max-review-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();

    let (cycle_id,): (String,) = sqlx::query_as("SELECT id FROM cycles WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let findings = warden::db::list_findings_for_cycle(&pool, &cycle_id)
        .await
        .unwrap();
    assert_eq!(
        findings.len(),
        2,
        "expected exactly one finding from each of reviewer and tester"
    );

    let reviewer_finding = findings
        .iter()
        .find(|f| f.source == FindingSource::role("reviewer"))
        .expect("reviewer finding present");
    let tester_finding = findings
        .iter()
        .find(|f| f.source == FindingSource::role("tester"))
        .expect("tester finding present");

    assert!(
        reviewer_finding
            .description
            .contains("test_target_seen=original-test"),
        "reviewer's worktree must still see the untouched original \
         test_target.txt, not the tester's write -- got: {}",
        reviewer_finding.description
    );
    assert!(
        tester_finding
            .description
            .contains("review_target_seen=original-review"),
        "tester's worktree must still see the untouched original \
         review_target.txt, not the reviewer's write -- got: {}",
        tester_finding.description
    );

    let (reviewer_wt,): (String,) =
        sqlx::query_as("SELECT worktree_path FROM cycle_worktrees WHERE cycle_id = ? AND role = ?")
            .bind(&cycle_id)
            .bind("reviewer")
            .fetch_one(&pool)
            .await
            .expect("reviewer worktree path recorded");
    let (tester_wt,): (String,) =
        sqlx::query_as("SELECT worktree_path FROM cycle_worktrees WHERE cycle_id = ? AND role = ?")
            .bind(&cycle_id)
            .bind("tester")
            .fetch_one(&pool)
            .await
            .expect("tester worktree path recorded");
    assert_ne!(
        reviewer_wt, tester_wt,
        "reviewer and tester must run in distinct worktree directories"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_crash_restart_leaves_no_orphan_worktree_or_process() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let db_path = warden_home.path().join("state.db");

    let (worktree_path, mut orphan_child) = {
        let pool = warden::db::connect(&db_path).await.unwrap();

        let worktree_manager = warden::worktree::WorktreeManager::new(
            repo.path(),
            warden_home.path().join("worktrees"),
        )
        .unwrap();
        let worktree = worktree_manager
            .create("orphan-e2e-run", "coder", "HEAD")
            .await
            .unwrap();
        let worktree_path = worktree.path().to_path_buf();
        std::mem::forget(worktree);
        assert!(
            worktree_path.exists(),
            "precondition: orphan worktree exists on disk"
        );

        warden::db::insert_run(
            &pool,
            "orphan-e2e-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        warden::db::update_run_state(&pool, "orphan-e2e-run", RunState::CoderRunning)
            .await
            .unwrap();
        warden::db::insert_cycle(&pool, "orphan-e2e-cycle", "orphan-e2e-run", 1)
            .await
            .unwrap();
        warden::db::set_cycle_worktree_path(
            &pool,
            "orphan-e2e-cycle",
            "coder",
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        let orphan_child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let orphan_pid = orphan_child.id().unwrap();
        warden::db::insert_agent_process(
            &pool,
            "orphan-e2e-live-process",
            "orphan-e2e-cycle",
            "reviewer",
            orphan_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let mut dead_child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = dead_child.id().unwrap();
        dead_child.wait().await.unwrap();
        warden::db::insert_agent_process(
            &pool,
            "orphan-e2e-dead-process",
            "orphan-e2e-cycle",
            "coder",
            dead_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        pool.close().await;
        (worktree_path, orphan_child)
    };

    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "unrelated new run",
            "--branch",
            "main",
            "--max-review-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let pool = warden::db::connect(&db_path).await.unwrap();
    let recovered = warden::db::get_run(&pool, "orphan-e2e-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.state, RunState::Failed);
    assert!(
        !worktree_path.exists(),
        "no orphan worktree may persist after a crash+restart cycle"
    );

    let exit_status = orphan_child.wait().await.unwrap();
    assert!(
        !exit_status.success(),
        "no orphan agent process may persist after a crash+restart cycle"
    );
    let open_processes = warden::db::list_open_agent_processes_for_run(&pool, "orphan-e2e-run")
        .await
        .unwrap();
    assert!(
        open_processes.is_empty(),
        "recovery must mark the orphaned agent_processes row ended"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_restart_backs_up_db_before_applying_pending_migrations_via_cli() {
    let warden_home = TempDir::new().unwrap();
    std::fs::create_dir_all(warden_home.path()).unwrap();
    let db_path = warden_home.path().join("state.db");

    {
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();
        static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
        let first_migration_version = MIGRATOR.iter().next().unwrap().version;
        MIGRATOR
            .run_to(first_migration_version, &pool)
            .await
            .unwrap();
        pool.close().await;
    }

    let repo = init_test_repo();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "trigger startup migration",
            "--branch",
            "main",
            "--max-review-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let backups: Vec<_> = std::fs::read_dir(warden_home.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak-"))
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "restarting against a pre-existing db with pending migrations must produce exactly one backup file: {backups:?}"
    );

    let pool = warden::db::connect(&db_path).await.unwrap();
    let (run_id,): (String,) = sqlx::query_as("SELECT id FROM runs LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::Converged);
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_coder_ignoring_a_large_stdin_payload_and_exiting_immediately_still_converges() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();

    write_fake_tool(
        bin_dir.path(),
        "claude",
        r#"#!/bin/sh
set -e
if [ -f notes.txt ]; then
    printf '{"type":"result","subtype":"success","is_error":false,"result":""}\n'
    exit 0
fi
echo hello >> notes.txt
git add notes.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
printf '{"type":"result","subtype":"success","is_error":false,"result":""}\n'
exit 0
"#,
    );

    let large_intent = format!("large intent payload: {}", "x".repeat(100_000));

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            &large_intent,
            "--branch",
            "main",
            "--max-review-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_prior_cycle_findings_from_a_reboucle_reach_the_next_cycles_agents_stdin() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    let coder_body = format!(
        r#"
n=$(ls "{captures}"/coder_stdin_*.json 2>/dev/null | wc -l | tr -d ' ')
next=$((n + 1))
cp "$stdin_file" "{captures}/coder_stdin_$next.json"
if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
    echo fixed > status.txt
else
    echo broken > status.txt
fi
git add status.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#,
        captures = captures.path().display()
    );
    let reviewer_body = format!(
        r#"
n=$(ls "{captures}"/reviewer_stdin_*.json 2>/dev/null | wc -l | tr -d ' ')
next=$((n + 1))
cp "$stdin_file" "{captures}/reviewer_stdin_$next.json"
if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
    printf '%s\n' '{{"source":"reviewer","severity":"blocking","description":"status is broken"}}' > "$WARDEN_RESULT_FILE"
fi
"#,
        captures = captures.path().display()
    );
    let tester_body = format!(
        r#"
n=$(ls "{captures}"/tester_stdin_*.json 2>/dev/null | wc -l | tr -d ' ')
next=$((n + 1))
cp "$stdin_file" "{captures}/tester_stdin_$next.json"
"#,
        captures = captures.path().display()
    );

    write_fake_claude(bin_dir.path(), &coder_body, &reviewer_body, &tester_body);

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "flip status to fixed via a reboucle",
            "--branch",
            "main",
            "--max-review-cycles",
            "5",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let cycle1: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("reviewer_stdin_1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        cycle1["findings"].as_array().unwrap().len(),
        0,
        "cycle 1 has no prior cycle, so the reviewer must see no prior findings"
    );

    let cycle2: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("reviewer_stdin_2.json")).unwrap(),
    )
    .unwrap();
    let cycle2_findings = cycle2["findings"].as_array().unwrap();
    assert_eq!(
        cycle2_findings.len(),
        1,
        "cycle 2's reviewer must receive exactly the one finding that triggered the reboucle"
    );
    assert_eq!(cycle2_findings[0]["source"], "reviewer");
    assert_eq!(cycle2_findings[0]["severity"], "blocking");
    assert_eq!(cycle2_findings[0]["description"], "status is broken");
    assert_ne!(
        cycle1["target_commit"], cycle2["target_commit"],
        "cycle 2 must be reviewing a different (later) commit than cycle 1"
    );

    assert!(
        !captures.path().join("tester_stdin_2.json").exists(),
        "the tester must never have run a second time -- it only ever ran once, in cycle 2"
    );
    let tester_first_invocation: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("tester_stdin_1.json")).unwrap(),
    )
    .unwrap();
    let tester_findings = tester_first_invocation["findings"].as_array().unwrap();
    assert_eq!(tester_findings.len(), 1);
    assert_eq!(tester_findings[0]["description"], "status is broken");
    assert_eq!(
        tester_first_invocation["target_commit"], cycle2["target_commit"],
        "the tester's one invocation must review the same (cycle 2) commit as the reviewer's \
         second pass, since it only ever runs once the review gate opens"
    );

    let coder_cycle1: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("coder_stdin_1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        coder_cycle1["findings"].as_array().unwrap().len(),
        0,
        "cycle 1 has no prior cycle, so the coder must see no findings to fix"
    );

    let coder_cycle2: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("coder_stdin_2.json")).unwrap(),
    )
    .unwrap();
    let coder_cycle2_findings = coder_cycle2["findings"].as_array().unwrap();
    assert_eq!(
        coder_cycle2_findings.len(),
        1,
        "cycle 2's coder must receive exactly the finding it is being asked to fix"
    );
    assert_eq!(coder_cycle2_findings[0]["source"], "reviewer");
    assert_eq!(coder_cycle2_findings[0]["severity"], "blocking");
    assert_eq!(coder_cycle2_findings[0]["description"], "status is broken");
    assert_eq!(
        coder_cycle2["intent"], "flip status to fixed via a reboucle",
        "the run intent must still reach the coder alongside its findings"
    );
    assert!(
        coder_cycle2["target_commit"].is_null(),
        "A2: the coder gets intent + findings only, never a target_commit"
    );
    assert!(
        coder_cycle2["diff"].is_null(),
        "A2: the coder reads its own worktree's diff rather than being sent one"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_non_ascii_multiline_prompt_and_intent_survive_the_stdin_round_trip() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    let prompt = "Tu es le codeur de Warden.\n\nRègles : « ne jamais » deviner 🤖\n\tIndenté.";
    let intent = "Ajouter le résumé « fin » — avec un tiret cadratin 🚀";
    write_agent_definition(repo.path(), "coder", "", prompt);

    write_fake_claude(
        bin_dir.path(),
        &format!(
            "cp \"$stdin_file\" \"{captures}/coder_stdin.json\"\n{APPEND_NOTES_CODER_BODY}",
            captures = captures.path().display()
        ),
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            intent,
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let payload: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("coder_stdin.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        payload["system_prompt"], prompt,
        "a non-ASCII multi-line prompt must reach the agent intact"
    );
    assert_eq!(
        payload["intent"], intent,
        "a non-ASCII intent must reach the agent intact"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_cli_project_selects_asciinema_and_evidence_is_stored_and_committed_by_default() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );
    write_fake_asciinema(bin_dir.path());

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "cli project captures evidence via asciinema",
            "--branch",
            "main",
            "--max-review-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();

    let evidence = warden::db::list_evidence_for_run(&pool, &run_id)
        .await
        .unwrap();
    assert_eq!(
        evidence.len(),
        1,
        "expected one evidence row captured by the fake asciinema tool"
    );
    assert_eq!(
        evidence[0].evidence.evidence_type,
        warden_core::EvidenceType::Other
    );
    assert_eq!(
        evidence[0].evidence.file_path,
        ".warden/evidence/1/session.cast"
    );

    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    let converged_sha = run
        .converged_commit_sha
        .expect("a converged run has a persisted commit sha");

    let show = SyncCommand::new("git")
        .current_dir(repo.path())
        .args([
            "show",
            &format!("{converged_sha}:.warden/evidence/1/session.cast"),
        ])
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "expected .warden/evidence/1/session.cast inside the converged commit"
    );

    let (cycle_sha,): (Option<String>,) =
        sqlx::query_as("SELECT coder_commit_sha FROM cycles WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(
        Some(converged_sha.as_str()),
        cycle_sha.as_deref(),
        "the converged commit must be a distinct evidence commit on top of the coder's own commit"
    );

    let status = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(status.stdout.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_web_project_marker_selects_playwright_and_evidence_is_committed() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();

    let coder_body = r#"
echo '<html></html>' > index.html
git add index.html
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#;
    write_fake_claude(bin_dir.path(), coder_body, NOOP_BODY, NOOP_BODY);
    write_fake_npx(bin_dir.path());

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "web project captures evidence via playwright",
            "--branch",
            "main",
            "--max-review-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let evidence = warden::db::list_evidence_for_run(&pool, &run_id)
        .await
        .unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(
        evidence[0].evidence.evidence_type,
        warden_core::EvidenceType::Image
    );
    assert!(evidence[0]
        .evidence
        .file_path
        .starts_with(".warden/evidence/1/"));
    assert!(evidence[0].evidence.file_path.ends_with("screenshot.png"));

    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    let converged_sha = run.converged_commit_sha.unwrap();
    let show = SyncCommand::new("git")
        .current_dir(repo.path())
        .args([
            "show",
            &format!("{converged_sha}:{}", evidence[0].evidence.file_path),
        ])
        .output()
        .unwrap();
    assert!(show.status.success());
    assert_eq!(show.stdout, b"fake-png-bytes".to_vec());
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_evidence_tool_override_wins_over_web_auto_detection() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();

    let coder_body = r#"
echo '<html></html>' > index.html
git add index.html
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#;
    write_fake_claude(bin_dir.path(), coder_body, NOOP_BODY, NOOP_BODY);
    write_fake_asciinema(bin_dir.path());

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "override forces asciinema on a web-looking project",
            "--branch",
            "main",
            "--max-review-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
            "--evidence-tool",
            "asciinema",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let evidence = warden::db::list_evidence_for_run(&pool, &run_id)
        .await
        .unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(
        evidence[0].evidence.file_path, ".warden/evidence/1/session.cast",
        "the config override must dispatch to asciinema, not Playwright, despite the web marker file"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_evidence_store_in_repo_false_keeps_evidence_local_and_never_commits_it() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );
    write_fake_asciinema(bin_dir.path());

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "evidence stays local when store-in-repo is disabled",
            "--branch",
            "main",
            "--max-review-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
            "--evidence-store-in-repo",
            "false",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();

    let evidence = warden::db::list_evidence_for_run(&pool, &run_id)
        .await
        .unwrap();
    assert_eq!(evidence.len(), 1);
    let scratch_path = warden_home
        .path()
        .join("evidence")
        .join(&run_id)
        .join("1")
        .join("session.cast");
    assert!(
        scratch_path.exists(),
        "evidence must still be staged on local scratch storage: {}",
        scratch_path.display()
    );

    let ref_lookup = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["rev-parse", &format!("refs/warden/runs/{run_id}/evidence")])
        .output()
        .unwrap();
    assert!(
        !ref_lookup.status.success(),
        "no evidence commit/ref may exist when store_in_repo is false"
    );

    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    let converged_sha = run.converged_commit_sha.unwrap();
    let (cycle_sha,): (Option<String>,) =
        sqlx::query_as("SELECT coder_commit_sha FROM cycles WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        Some(converged_sha.as_str()),
        cycle_sha.as_deref(),
        "with store_in_repo=false the converged commit must be exactly the coder's commit"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_evidence_capture_failure_when_tool_missing_is_non_fatal_and_run_still_converges() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "converges even though no evidence tool is installed",
            "--branch",
            "main",
            "--max-review-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"))
        .stdout(contains("evidence capture failed"));

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let (run_id,): (String,) = sqlx::query_as("SELECT id FROM runs LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();

    let evidence = warden::db::list_evidence_for_run(&pool, &run_id)
        .await
        .unwrap();
    assert!(
        evidence.is_empty(),
        "no evidence row should exist when the capture tool is unavailable"
    );

    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::Converged);
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_tui_flag_spawns_the_configured_binary_with_run_id_and_warden_home() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    let tui_dir = TempDir::new().unwrap();
    let captured_argv = tui_dir.path().join("captured-argv.txt");
    let fake_tui = write_fake_tool(
        tui_dir.path(),
        "fake-warden-tui",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nexec 1>&- 2>&-\nsleep 30\n",
            captured_argv.display()
        ),
    );

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "issue 32: --tui spawn wiring",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
            "--tui",
            "--tui-bin",
            fake_tui.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let captured = std::fs::read_to_string(&captured_argv).unwrap();

    assert_eq!(
        captured.lines().collect::<Vec<_>>(),
        vec![
            "attach",
            "--run-id",
            &run_id,
            "--warden-home",
            warden_home.path().to_str().unwrap(),
        ],
        "the spawned warden-tui must receive exactly the attach subcommand for this run"
    );
}

#[cfg(unix)]
#[test]
fn e2e_tui_flag_does_not_block_on_a_still_running_tui_when_stdout_is_not_a_terminal() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    let tui_dir = TempDir::new().unwrap();
    let fake_tui = write_fake_tool(tui_dir.path(), "fake-warden-tui", "#!/bin/sh\nsleep 30\n");

    let mut child = SyncCommand::new(env!("CARGO_BIN_EXE_warden"))
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "issue 32: warden run must not block on a headless warden-tui",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
            "--tui",
            "--tui-bin",
            fake_tui.to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut sink = Vec::new();
        let _ = stdout.read_to_end(&mut sink);
    });
    std::thread::spawn(move || {
        use std::io::Read;
        let mut sink = Vec::new();
        let _ = stderr.read_to_end(&mut sink);
    });

    let started = std::time::Instant::now();
    let status = child.wait().unwrap();
    let elapsed = started.elapsed();

    assert!(
        status.success(),
        "warden run itself must exit successfully regardless of the still-running TUI"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "warden run must not wait for a non-tty warden-tui to exit -- it never will on its own \
         within this test's bound (elapsed: {elapsed:?}, fake tui sleeps 30s)"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_tui_exit_cancels_a_still_running_run() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(bin_dir.path(), "sleep 30", NOOP_BODY, NOOP_BODY);

    let tui_dir = TempDir::new().unwrap();
    let fake_tui = write_fake_tool(tui_dir.path(), "fake-warden-tui", "#!/bin/sh\nexit 0\n");

    let started = std::time::Instant::now();
    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "issue 32: TUI exit cancels the run",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
            "--tui",
            "--tui-bin",
            fake_tui.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("was cancelled"));
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "the run must be cancelled promptly once the TUI exits, not run to completion \
         (elapsed: {elapsed:?}, coder sleeps 30s)"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_tui_spawn_failure_aborts_the_run_instead_of_degrading_to_headless() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(bin_dir.path(), "sleep 30", NOOP_BODY, NOOP_BODY);

    let tui_dir = TempDir::new().unwrap();
    let missing_tui_bin = tui_dir.path().join("does-not-exist");

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "issue 32: --tui spawn failure must abort the run",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
            "--tui",
            "--tui-bin",
            missing_tui_bin.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("failed to spawn"))
        .stderr(contains(missing_tui_bin.to_str().unwrap().to_string()));
}

fn write_workflow_yaml(repo: &Path, yaml: &str) {
    let dir = repo.join(".warden");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("workflow.yaml"), yaml).unwrap();
}

fn write_custom_step_agent_definition(repo: &Path, agent_name: &str, system_prompt: &str) {
    let dir = repo.join(".claude").join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{agent_name}.md")),
        format!("---\n---\n\n{system_prompt}\n"),
    )
    .unwrap();
}

const DEFAULT_WORKFLOW_YAML: &str = r#"
name: default
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: code-reviewer
    gate: loop-until-clean
  - role: tester
    agent: test-runner
    gate: loop-until-clean
"#;

#[cfg(unix)]
#[tokio::test]
async fn e2e_an_explicit_workflow_yaml_reproducing_the_default_shape_converges_like_the_builtin_one(
) {
    let repo = init_test_repo();
    write_workflow_yaml(repo.path(), DEFAULT_WORKFLOW_YAML);
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "explicit default-shaped workflow.yaml",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_no_workflow_file_present_reproduces_the_pre_issue_73_pipeline() {
    let repo = init_test_repo();
    assert!(
        !repo.path().join(".warden").join("workflow.yaml").exists(),
        "this test's own premise: no workflow.yaml at all"
    );
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        FLIP_STATUS_CODER_BODY,
        STATUS_GATED_REVIEWER_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "no workflow.yaml at all",
            "--max-review-cycles",
            "5",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_a_custom_techlead_role_actually_gates_the_pipeline_and_its_findings_aggregate() {
    let repo = init_test_repo();
    let yaml = r#"
name: with-techlead
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: code-reviewer
    gate: loop-until-clean
  - role: tester
    agent: test-runner
    gate: loop-until-clean
  - role: techlead
    agent: techlead
    gate: loop-until-clean
"#;
    write_workflow_yaml(repo.path(), yaml);
    write_custom_step_agent_definition(
        repo.path(),
        "techlead",
        "You are Warden's tech lead: arbitrate reviewer/tester findings and give a go/no-go.",
    );

    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let techlead_marker = bin_dir.path().join("techlead-has-run-once");
    let script = format!(
        r#"#!/bin/sh
set -e
stdin_file=$(mktemp)
cat > "$stdin_file"
WARDEN_RESULT_FILE=$(mktemp)
export WARDEN_RESULT_FILE
: > "$WARDEN_RESULT_FILE"

if grep -q '"role":"coder"' "$stdin_file"; then
{APPEND_NOTES_CODER_BODY}
elif grep -q '"role":"reviewer"' "$stdin_file"; then
{NOOP_BODY}
elif grep -q '"role":"tester"' "$stdin_file"; then
{NOOP_BODY}
else
    if [ -f "{marker}" ]; then
        true
    else
        touch "{marker}"
        printf '%s\n' '{{"source":"techlead","severity":"blocking","description":"needs another pass"}}' > "$WARDEN_RESULT_FILE"
    fi
fi

result=$(cat "$WARDEN_RESULT_FILE")
rm -f "$WARDEN_RESULT_FILE" "$stdin_file"
escaped=$(printf '%s' "$result" | python3 -c 'import json,sys; sys.stdout.write(json.dumps(sys.stdin.read()))')
printf '{{"type":"result","subtype":"success","is_error":false,"result":%s}}\n' "$escaped"
"#,
        marker = techlead_marker.display()
    );
    write_fake_tool(bin_dir.path(), "claude", &script);

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "exercise the custom techlead role",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    assert!(
        techlead_marker.exists(),
        "the techlead role must actually have run as a real subprocess"
    );

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();

    let cycle_ids: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM cycles WHERE run_id = ? ORDER BY cycle_number")
            .bind(&run_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        cycle_ids.len(),
        2,
        "the techlead's first blocking finding must reboucle to a second cycle"
    );

    let mut all_findings = Vec::new();
    for (cycle_id,) in &cycle_ids {
        all_findings.extend(
            warden::db::list_findings_for_cycle(&pool, cycle_id)
                .await
                .unwrap(),
        );
    }
    let techlead_finding = all_findings
        .iter()
        .find(|finding| finding.source == FindingSource::role("techlead"))
        .expect("the techlead's own finding must be persisted, aggregating like any other role's");
    assert_eq!(techlead_finding.description, "needs another pass");
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_a_type_hook_step_gates_the_pipeline_via_a_deterministic_command() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let lint_marker = bin_dir.path().join("lint-has-run");

    let yaml = format!(
        r#"
name: with-lint-hook
steps:
  - role: coder
    agent: coder
  - role: lint
    type: hook
    run: "touch '{}'"
    gate: loop-until-clean
"#,
        lint_marker.display()
    );
    write_workflow_yaml(repo.path(), &yaml);
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "exercise a type: hook workflow step",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    assert!(
        lint_marker.exists(),
        "the type: hook step's shell command must actually have run, as a real deterministic \
         action -- not an agent subprocess"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_a_malformed_workflow_yaml_is_a_clean_cli_error_naming_the_file() {
    let repo = init_test_repo();
    write_workflow_yaml(repo.path(), "name: x\nsteps: []\n");
    let warden_home = TempDir::new().unwrap();

    warden_command()
        .0
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "malformed workflow file",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stderr(contains("workflow.yaml"))
        .stderr(contains("at least one step"));
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_an_unresolvable_custom_step_agent_is_a_clean_cli_error_naming_the_role_and_path() {
    let repo = init_test_repo();
    let yaml = r#"
name: with-techlead
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: code-reviewer
    gate: loop-until-clean
  - role: tester
    agent: test-runner
    gate: loop-until-clean
  - role: techlead
    agent: techlead
    gate: loop-until-clean
"#;
    write_workflow_yaml(repo.path(), yaml);
    let warden_home = TempDir::new().unwrap();

    warden_command()
        .0
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "unresolvable custom step agent",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stderr(contains("techlead"))
        .stderr(contains(
            repo.path()
                .join(".claude")
                .join("agents")
                .join("techlead.md")
                .to_str()
                .unwrap()
                .to_string(),
        ));
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_a_custom_step_inserted_between_reviewer_and_tester_actually_runs() {
    let repo = init_test_repo();
    let yaml = r#"
name: techlead-in-the-middle
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: code-reviewer
    gate: loop-until-clean
  - role: techlead
    agent: techlead
    gate: loop-until-clean
  - role: tester
    agent: test-runner
    gate: loop-until-clean
"#;
    write_workflow_yaml(repo.path(), yaml);
    write_custom_step_agent_definition(
        repo.path(),
        "techlead",
        "You are Warden's tech lead, running between the reviewer and the tester.",
    );

    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let techlead_marker = bin_dir.path().join("techlead-has-run");
    let script = format!(
        r#"#!/bin/sh
set -e
stdin_file=$(mktemp)
cat > "$stdin_file"
WARDEN_RESULT_FILE=$(mktemp)
export WARDEN_RESULT_FILE
: > "$WARDEN_RESULT_FILE"

if grep -q '"role":"coder"' "$stdin_file"; then
{APPEND_NOTES_CODER_BODY}
elif grep -q '"role":"reviewer"' "$stdin_file"; then
{NOOP_BODY}
elif grep -q '"role":"techlead"' "$stdin_file"; then
    touch "{marker}"
else
{NOOP_BODY}
fi

result=$(cat "$WARDEN_RESULT_FILE")
rm -f "$WARDEN_RESULT_FILE" "$stdin_file"
escaped=$(printf '%s' "$result" | python3 -c 'import json,sys; sys.stdout.write(json.dumps(sys.stdin.read()))')
printf '{{"type":"result","subtype":"success","is_error":false,"result":%s}}\n' "$escaped"
"#,
        marker = techlead_marker.display()
    );
    write_fake_tool(bin_dir.path(), "claude", &script);

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "insert techlead between reviewer and tester",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    assert!(
        techlead_marker.exists(),
        "the mid-pipeline techlead step must actually have run as a real subprocess"
    );

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);
    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let entries = warden::db::list_cycle_worktree_entries_for_run(&pool, &run_id)
        .await
        .unwrap();
    let roles_seen: Vec<String> = entries.into_iter().map(|entry| entry.role).collect();
    for expected_role in ["coder", "reviewer", "techlead", "tester"] {
        assert!(
            roles_seen.iter().any(|role| role == expected_role),
            "expected role {expected_role:?} to have its own worktree recorded, got: {roles_seen:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_a_custom_steps_worktree_and_process_are_reclaimed_by_crash_recovery() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let db_path = warden_home.path().join("state.db");

    let (worktree_path, mut orphan_child) = {
        let pool = warden::db::connect(&db_path).await.unwrap();

        let worktree_manager = warden::worktree::WorktreeManager::new(
            repo.path(),
            warden_home.path().join("worktrees"),
        )
        .unwrap();
        let worktree = worktree_manager
            .create("techlead-orphan-run", "techlead", "HEAD")
            .await
            .unwrap();
        let worktree_path = worktree.path().to_path_buf();
        std::mem::forget(worktree);
        assert!(
            worktree_path.exists(),
            "precondition: the custom step's orphan worktree exists on disk"
        );

        warden::db::insert_run(
            &pool,
            "techlead-orphan-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
            3,
            4,
            5,
        )
        .await
        .unwrap();
        warden::db::update_run_state(&pool, "techlead-orphan-run", RunState::RunningStep(3))
            .await
            .unwrap();
        warden::db::insert_cycle(&pool, "techlead-orphan-cycle", "techlead-orphan-run", 1)
            .await
            .unwrap();
        warden::db::set_cycle_worktree_path(
            &pool,
            "techlead-orphan-cycle",
            "techlead",
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        let orphan_child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let orphan_pid = orphan_child.id().unwrap();
        warden::db::insert_agent_process(
            &pool,
            "techlead-orphan-live-process",
            "techlead-orphan-cycle",
            "techlead",
            orphan_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let mut dead_child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = dead_child.id().unwrap();
        dead_child.wait().await.unwrap();
        warden::db::insert_agent_process(
            &pool,
            "techlead-orphan-dead-process",
            "techlead-orphan-cycle",
            "techlead",
            dead_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        pool.close().await;
        (worktree_path, orphan_child)
    };

    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "unrelated new run",
            "--branch",
            "main",
            "--max-review-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let pool = warden::db::connect(&db_path).await.unwrap();
    let recovered = warden::db::get_run(&pool, "techlead-orphan-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.state, RunState::Failed);
    assert!(
        !worktree_path.exists(),
        "no custom step's orphan worktree may persist after a crash+restart cycle"
    );

    let exit_status = orphan_child.wait().await.unwrap();
    assert!(
        !exit_status.success(),
        "no custom step's orphan agent process may persist after a crash+restart cycle"
    );
    let open_processes =
        warden::db::list_open_agent_processes_for_run(&pool, "techlead-orphan-run")
            .await
            .unwrap();
    assert!(
        open_processes.is_empty(),
        "recovery must mark the custom step's orphaned agent_processes row ended"
    );
}

fn extract_all_finished_lines(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            line.strip_prefix("run ")
                .and_then(|rest| rest.split_once(" finished: "))
                .map(|(id, state)| (id.to_string(), state.to_string()))
        })
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_batch_three_intents_each_converge_as_their_own_isolated_run() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "batch intent one",
            "--intent",
            "batch intent two",
            "--intent",
            "batch intent three",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("batch summary: 3/3 intent(s) converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let finished = extract_all_finished_lines(&stdout);
    assert_eq!(
        finished.len(),
        3,
        "expected exactly 3 \"finished:\" lines, one per intent, got: {stdout:?}"
    );
    assert!(
        finished.iter().all(|(_, state)| state == "Converged"),
        "every intent must converge: {finished:?}"
    );

    let mut run_ids: Vec<&str> = finished.iter().map(|(id, _)| id.as_str()).collect();
    run_ids.sort_unstable();
    run_ids.dedup();
    assert_eq!(
        run_ids.len(),
        3,
        "expected 3 distinct run ids: {finished:?}"
    );

    let pool = warden::db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    for (run_id, _) in &finished {
        let run = warden::db::get_run(&pool, run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Converged);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_batch_prints_the_isolation_warning_once_per_child_never_on_stdout() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "batch warning intent one",
            "--intent",
            "batch warning intent two",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("batch summary: 2/2 intent(s) converged"))
        .stdout(contains("ADR-0021").not());

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let warning_count = stderr.matches("ADR-0021").count();
    assert_eq!(
        warning_count, 2,
        "expected exactly one isolation warning per batch child (2 intents), got {warning_count} in stderr: {stderr:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_batch_continues_past_a_non_converged_intent_by_default() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let coder_body_writes_its_own_intent_into_the_commit = r#"
intent=$(python3 -c "import json; print(json.load(open('$stdin_file'))['intent'])")
printf '%s\n' "$intent" >> notes.txt
git add notes.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#;
    write_fake_claude(
        bin_dir.path(),
        coder_body_writes_its_own_intent_into_the_commit,
        r#"if grep -q 'BATCH_FORCE_FAIL_MARKER' "$stdin_file"; then
    printf '%s\n' '{"source":"reviewer","severity":"blocking","description":"forced failure"}' > "$WARDEN_RESULT_FILE"
fi"#,
        NOOP_BODY,
    );

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "first intent converges",
            "--intent",
            "second intent BATCH_FORCE_FAIL_MARKER never converges",
            "--intent",
            "third intent converges",
            "--max-review-cycles",
            "1",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stdout(contains("batch summary: 2/3 intent(s) converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let finished = extract_all_finished_lines(&stdout);
    assert_eq!(
        finished.len(),
        3,
        "all 3 intents must have run (continue-by-default), got: {stdout:?}"
    );
    assert_eq!(finished[0].1, "Converged");
    assert_eq!(finished[1].1, "StepCyclesExceeded(1)");
    assert_eq!(finished[2].1, "Converged");
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_batch_fail_fast_stops_at_the_first_non_converged_intent() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        r#"printf '%s\n' '{"source":"reviewer","severity":"blocking","description":"always blocking"}' > "$WARDEN_RESULT_FILE""#,
        NOOP_BODY,
    );

    let assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "never converges (always blocking reviewer)",
            "--intent",
            "would converge, but must be skipped",
            "--max-review-cycles",
            "1",
            "--fail-fast",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stdout(contains(
            "SKIPPED -- earlier intent failed under --fail-fast",
        ));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let finished = extract_all_finished_lines(&stdout);
    assert_eq!(
        finished.len(),
        1,
        "the second intent must never have been attempted under --fail-fast: {stdout:?}"
    );
    assert_eq!(finished[0].1, "StepCyclesExceeded(1)");
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_batch_combines_intents_file_entries_with_repeated_intent_flags() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    let intents_file = warden_home.path().join("intents.txt");
    std::fs::write(
        &intents_file,
        "# a comment, ignored\nfrom file: first\n\nfrom file: second\n",
    )
    .unwrap();

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intents-file",
            intents_file.to_str().unwrap(),
            "--intent",
            "from flag: third",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("batch summary: 3/3 intent(s) converged"))
        .stdout(contains("[1/3] \"from file: first\""))
        .stdout(contains("[2/3] \"from file: second\""))
        .stdout(contains("[3/3] \"from flag: third\""));
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_run_without_any_intent_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();

    warden_command()
        .0
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stderr(contains("no intent provided"));
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_run_with_an_all_comment_intents_file_names_the_file_in_the_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();

    let intents_file = warden_home.path().join("empty-intents.txt");
    std::fs::write(&intents_file, "# nothing but comments\n\n   \n").unwrap();

    warden_command()
        .0
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intents-file",
            intents_file.to_str().unwrap(),
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .failure()
        .stderr(contains("contained no intents"))
        .stderr(contains(intents_file.to_str().unwrap().to_string()));
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_batch_ctrl_c_lets_the_in_flight_intent_finish_then_skips_the_rest() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();

    let coder_body_sleeps_only_for_the_marked_intent = r#"
intent=$(python3 -c "import json; print(json.load(open('$stdin_file'))['intent'])")
case "$intent" in
    *SLOW_FIRST_INTENT*) sleep 2 ;;
esac
echo hello >> notes.txt
git add notes.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#;
    write_fake_claude(
        bin_dir.path(),
        coder_body_sleeps_only_for_the_marked_intent,
        NOOP_BODY,
        NOOP_BODY,
    );

    let bin_path = env!("CARGO_BIN_EXE_warden");
    let mut child = SyncCommand::new(bin_path)
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "first SLOW_FIRST_INTENT intent",
            "--intent",
            "second intent, must be skipped",
            "--intent",
            "third intent, must be skipped",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn warden");

    let pid = child.id();
    let child_stdout = child.stdout.take().expect("piped stdout");
    let child_stderr = child.stderr.take().expect("piped stderr");

    let stderr_thread = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let mut stderr = child_stderr;
        stderr.read_to_string(&mut buf).ok();
        buf
    });

    let stdout_lines: Vec<String> = {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(child_stdout);
        let mut lines = Vec::new();
        let mut sent_sigint = false;
        for line in reader.lines() {
            let line = line.expect("read batch stdout line");
            let is_started_line = line.ends_with(" started");
            lines.push(line);
            if is_started_line && !sent_sigint {
                let status = SyncCommand::new("kill")
                    .args(["-INT", &pid.to_string()])
                    .status()
                    .expect("send SIGINT to the batch parent");
                assert!(status.success(), "`kill -INT {pid}` must succeed");
                sent_sigint = true;
            }
        }
        lines
    };

    let status = child.wait().expect("wait for warden to exit");
    let stderr_output = stderr_thread.join().expect("stderr thread");
    let stdout = stdout_lines.join("\n");

    assert!(
        !status.success(),
        "the batch must exit non-zero: two of its three intents were skipped rather than \
         converged (stdout: {stdout:?}, stderr: {stderr_output:?})"
    );

    let finished = extract_all_finished_lines(&stdout);
    assert_eq!(
        finished.len(),
        1,
        "only the in-flight first intent should ever reach a \"finished:\" line; the second \
         and third must never have been started: {stdout:?}"
    );
    assert_eq!(
        finished[0].1, "Converged",
        "the in-flight intent must be left to finish (and converge) rather than being killed \
         outright: {stdout:?}"
    );

    assert!(
        stdout.contains("batch summary: 1/3 intent(s) converged"),
        "the batch summary must still be printed after a Ctrl-C cancellation: {stdout:?}"
    );
    let skipped_for_cancellation = stdout
        .matches("SKIPPED -- batch was cancelled (Ctrl-C)")
        .count();
    assert_eq!(
        skipped_for_cancellation, 2,
        "both the second and third intents must be recorded as skipped due to cancellation, \
         not attempted: {stdout:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_startup_awaits_quota_resume_with_the_stored_adapter_before_preflight_failure() {
    const QUOTA_CODER_BODY: &str = r#"
printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1,"rateLimitType":"seven_day","utilization":0.95,"isUsingOverage":false,"surpassedThreshold":0.75}}'
echo quota-resume > quota.txt
git add quota.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "quota suspension"
"#;

    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let resume_marker = warden_home.path().join("stored-claude-resume.txt");
    let reviewer_body = format!("printf '%s\\n' reviewer >> '{}'", resume_marker.display());
    let tester_body = format!("printf '%s\\n' tester >> '{}'", resume_marker.display());
    write_fake_claude(
        bin_dir.path(),
        QUOTA_CODER_BODY,
        &reviewer_body,
        &tester_body,
    );

    let first_assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "suspend before reviewer",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: AwaitingQuotaReset"));
    let first_stdout = String::from_utf8(first_assert.get_output().stdout.clone()).unwrap();
    let suspended_run_id = first_stdout
        .lines()
        .find_map(|line| {
            line.strip_prefix("run ")
                .and_then(|line| line.strip_suffix(" started"))
        })
        .unwrap();
    assert!(
        !resume_marker.exists(),
        "the first run must suspend before invoking reviewer or tester"
    );

    let workflow_dir = repo.path().join(".warden");
    std::fs::create_dir_all(&workflow_dir).unwrap();
    std::fs::write(
        workflow_dir.join("workflow.yaml"),
        "this is not a valid workflow",
    )
    .unwrap();

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "foreground must fail preflight",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "mistral",
        ])
        .assert()
        .failure()
        .stderr(contains("invalid workflow file"));

    let pool = warden::db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    assert_eq!(
        warden::db::get_run(&pool, suspended_run_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        RunState::Converged
    );
    assert!(warden::db::get_quota_continuation(&pool, suspended_run_id)
        .await
        .unwrap()
        .is_none());
    let (run_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM runs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        run_count, 1,
        "startup recovery must continue the suspended run instead of creating a new run"
    );
    assert_eq!(
        std::fs::read_to_string(&resume_marker).unwrap(),
        "reviewer\ntester\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_startup_re_suspends_the_original_run_when_quota_is_still_unavailable() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let resumed_coder_marker = warden_home.path().join("resumed-coder-marker");
    let coder_body = format!(
        r#"
if [ -f '{marker}' ]; then
    printf '%s\n' '{{"type":"rate_limit_event","rate_limit_info":{{"status":"allowed_warning","resetsAt":2000000000,"rateLimitType":"seven_day","utilization":0.95,"isUsingOverage":false,"surpassedThreshold":0.75}}}}'
else
    printf '%s\n' '{{"type":"rate_limit_event","rate_limit_info":{{"status":"allowed_warning","resetsAt":1,"rateLimitType":"seven_day","utilization":0.95,"isUsingOverage":false,"surpassedThreshold":0.75}}}}'
fi
"#,
        marker = resumed_coder_marker.display(),
    );
    let reviewer_body = format!(
        r#"
printf '%s\n' '{{"source":"reviewer","severity":"blocking","description":"retry coder after quota reset"}}' > "$WARDEN_RESULT_FILE"
touch '{marker}'
"#,
        marker = resumed_coder_marker.display(),
    );
    write_fake_claude(bin_dir.path(), &coder_body, &reviewer_body, NOOP_BODY);

    let first_assert = warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "suspend before reviewer",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: AwaitingQuotaReset"));
    let first_stdout = String::from_utf8(first_assert.get_output().stdout.clone()).unwrap();
    let suspended_run_id = first_stdout
        .lines()
        .find_map(|line| {
            line.strip_prefix("run ")
                .and_then(|line| line.strip_suffix(" started"))
        })
        .unwrap();

    let workflow_dir = repo.path().join(".warden");
    std::fs::create_dir_all(&workflow_dir).unwrap();
    std::fs::write(
        workflow_dir.join("workflow.yaml"),
        "this is not a valid workflow",
    )
    .unwrap();

    warden_command()
        .0
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .env("XDG_CONFIG_HOME", warden_home.path())
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "foreground must fail preflight",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "mistral",
        ])
        .assert()
        .failure()
        .stderr(contains("invalid workflow file"));

    let pool = warden::db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    assert_eq!(
        warden::db::get_run(&pool, suspended_run_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        RunState::AwaitingQuotaReset {
            resets_at: 2_000_000_000,
        }
    );
    assert!(warden::db::get_quota_continuation(&pool, suspended_run_id)
        .await
        .unwrap()
        .is_some());
    let (run_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM runs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(run_count, 1);
}
