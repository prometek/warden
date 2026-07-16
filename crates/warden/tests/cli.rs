//! End-to-end tests driving the actual `warden` binary as a user/CI caller
//! would (`warden run --repo ... --intent ... --tool claude`), not the
//! internal `Orchestrator` API directly. These exercise the acceptance
//! criteria from issue #1 and issue #24 through the real entry point: CLI
//! arg parsing (`main.rs`), agent definition resolution, the convergence
//! loop, and the SQLite state left behind -- the same path a human invoking
//! `warden run` from a shell hits.
//!
//! # The fake `claude` harness (issue #24)
//!
//! `--tool claude` always execs the literal `claude` binary
//! (`ClaudeAdapter::build_command`) -- there is no more warden-native
//! "any program/args" escape hatch a definition file could name (that's
//! exactly what this issue removes, ADR-0013 / Q1/Q4). To drive a real
//! convergence loop deterministically, without a real Claude Code install,
//! an API key, or a network call (code-standards.md: tests must be
//! deterministic, no real network calls), [`write_fake_claude`] places a
//! fake `claude` executable earlier on `PATH` than the real one
//! (`path_with_fake_bin_first`, the same technique already used below for
//! `asciinema`/`npx`). Since every role invokes the *same* program, the fake
//! script tells roles apart the only way it can from a subprocess's own
//! point of view: by inspecting the `role` field of the
//! `AgentInputMessage` JSON payload `warden` still feeds it on stdin
//! (ADR-0012, unchanged by issue #24) -- not by argv, which is identical in
//! shape across roles.
//!
//! The fake script wraps whatever text a test's role-specific fragment
//! writes to `$WARDEN_RESULT_FILE` into the exact JSON envelope shape the
//! real `claude --output-format json` emits
//! (`{"type":"result","subtype":"success","is_error":false,"result":"..."}`,
//! verified directly against the real CLI -- see `tool_adapter.rs`'s own
//! module docs), so `ClaudeAdapter::extract_findings` round-trips through
//! this fixture exactly like it would the real binary.

use std::path::{Path, PathBuf};
use std::process::Command as SyncCommand;

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;
use warden_core::RunState;

/// Sets up a throwaway git repo with a single commit, suitable as `--repo`.
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

/// Writes `<repo>/.warden/agents/<role>.md` (issue #24 point 3's convention
/// file). `frontmatter` is the raw YAML block content (each line already
/// including its own trailing newline, e.g. `"model: opus\n"`) -- pass `""`
/// for "every frontmatter key omitted", which still produces a valid,
/// empty-but-present block (`---\n---\n`; see `agent_def.rs`'s own
/// `every_frontmatter_key_is_optional` test).
fn write_agent_definition(repo: &Path, role: &str, frontmatter: &str, system_prompt: &str) {
    let agents_dir = repo.join(".warden").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join(format!("{role}.md")),
        format!("---\n{frontmatter}---\n\n{system_prompt}\n"),
    )
    .unwrap();
}

/// See this module's own docs. `coder_body`/`reviewer_body`/`tester_body`
/// run with `cwd` already the role's own worktree
/// (`ClaudeAdapter`/`process::spawn`, unchanged) and the raw
/// `AgentInputMessage` JSON payload captured at `"$stdin_file"` -- so a
/// coder fragment can `git commit` exactly like the old direct-script
/// fixtures did, and a reviewer/tester fragment can grep `$stdin_file` for
/// context. Each must write its final-answer text (verbatim, unescaped) to
/// `"$WARDEN_RESULT_FILE"` before returning; leaving it empty is a
/// legitimate "no findings"/"nothing to say" answer.
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

/// A coder that flips `status.txt` between "broken" and "fixed" each time it
/// runs, paired with [`STATUS_GATED_REVIEWER_BODY`] -- deterministically
/// exercises exactly one reboucle before converging, without depending on a
/// real AI agent.
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

/// `fake_bin_dir` prepended onto the current process's real `PATH`, so a
/// fake tool placed there is found first while `git`/`sh`/coreutils still
/// resolve normally through the rest of the real `PATH`.
#[cfg(unix)]
fn path_with_fake_bin_first(fake_bin_dir: &Path) -> String {
    let real_path = std::env::var("PATH").unwrap_or_default();
    format!("{}:{real_path}", fake_bin_dir.display())
}

/// Extracts the run id `warden run`'s final stdout line
/// (`run <uuid> finished: <State>`) so the test can look the run up in
/// SQLite afterwards.
fn extract_run_id(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("could not find run id in stdout: {stdout:?}"))
        .to_string()
}

/// Acceptance criterion 1 (issue #1): "Un cycle complet (coder -> review ->
/// test -> reboucle si besoin) est reproductible sur un repo de test" --
/// driven through the real `warden run --tool claude` CLI command, exactly
/// as a user would invoke it, with a coder that only converges after one
/// reboucle.
///
/// Acceptance criterion 3 is also verified here (isolation): the main repo's
/// git history/working tree must be untouched by the run.
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

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "flip status to fixed",
            "--branch",
            "main",
            "--max-cycles",
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

/// The target UX issue #24 exists to enable: `--repo`, `--intent`, `--tool`
/// -- nothing else, zero `.warden/agents/*.md` at all. Every role falls back
/// to `ClaudeAdapter::default_prompt`.
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

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
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

/// M2 (issue #20 review), unaffected by issue #24: a coder that exits
/// non-zero must fail the run outright, never silently proceed to review as
/// if nothing happened.
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

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
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

/// M2 (issue #20 review): `--intent ""` must be a clean CLI parse error,
/// never a run that gets far enough to write a `runs` row before failing.
#[test]
fn e2e_blank_intent_is_a_clean_cli_error_and_creates_no_run_row() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();

    Command::cargo_bin("warden")
        .unwrap()
        .args([
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

    Command::cargo_bin("warden")
        .unwrap()
        .args([
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

/// `worktree::WorktreeManager::new` rejects a `--repo` with no `.git` --
/// this must surface as a clean CLI failure, not a panic.
#[test]
fn e2e_non_git_repo_path_is_a_clean_cli_error() {
    let not_a_repo = TempDir::new().unwrap();
    let warden_home = TempDir::new().unwrap();

    Command::cargo_bin("warden")
        .unwrap()
        .args([
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

/// Issue #24 point 1: `--tool` is validated against a closed, compiled-in
/// set at the CLI boundary (code-standards.md: "valider toute entrée
/// externe... à la frontière") -- an unsupported value is a clean parse
/// error naming what was given, never silently defaulted to whatever
/// adapter happens to exist.
#[test]
fn e2e_an_unknown_tool_is_a_clean_cli_error_naming_the_value() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();

    Command::cargo_bin("warden")
        .unwrap()
        .args([
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

/// `--tool` has no default: the target UX (issue #24) shows it explicitly on
/// every example invocation, so omitting it entirely must be a clean parse
/// error, not a silent fallback to whichever adapter happens to be first.
#[test]
fn e2e_omitting_tool_entirely_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();

    Command::cargo_bin("warden")
        .unwrap()
        .args([
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

/// Issue #24 point 4: the flags this issue removes (`--coder-agent`/
/// `--reviewer-agent`/`--tester-agent`, themselves the replacement for the
/// `--*-cmd` flags ADR-0012 already removed) must make the CLI *fail*, not
/// be silently ignored -- the worst outcome of a breaking migration is a
/// user's path being silently dropped while the run proceeds with defaults
/// instead.
#[test]
fn e2e_the_removed_agent_flags_are_rejected_by_the_cli_not_silently_ignored() {
    for removed_flag in ["--coder-agent", "--reviewer-agent", "--tester-agent"] {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();

        Command::cargo_bin("warden")
            .unwrap()
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

/// Q3 (ADR-0013, carried over by issue #24's new schema): an unknown
/// frontmatter key in a convention file is a clean CLI error naming the key,
/// discovered before any agent is spawned.
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

    Command::cargo_bin("warden")
        .unwrap()
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
        .stderr(contains("timeout"));
}

#[test]
fn e2e_an_agent_definition_with_a_blank_system_prompt_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    write_agent_definition(repo.path(), "coder", "", "   ");

    Command::cargo_bin("warden")
        .unwrap()
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
        .stderr(contains("blank"));
}

/// LOW (carried over from ADR-0013's own review): a CRLF definition file's
/// first line visibly *is* `---`, so the error must name the real cause
/// rather than a misleading "missing fence" complaint.
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

    Command::cargo_bin("warden")
        .unwrap()
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
        .stderr(contains("CRLF"));
}

/// A convention file that exists but fails to read for a reason other than
/// "doesn't exist" (here: it's a directory) must be a clean CLI error, not a
/// silent fallback to the adapter's default prompt.
#[test]
fn e2e_a_definition_path_that_is_a_directory_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    std::fs::create_dir_all(repo.path().join(".warden/agents/coder.md")).unwrap();

    Command::cargo_bin("warden")
        .unwrap()
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
        .failure();
}

/// ADR-0012, unchanged by issue #24: the reviewer/tester still receive
/// `target_commit`/`diff`/`role` on stdin -- the `ClaudeAdapter` only owns
/// the invocation's argv, never this channel.
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

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
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

/// ADR-0012, unchanged by issue #24: the coder still receives the run
/// intent as a versioned, role-tagged JSON payload on stdin.
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

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
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

/// Architecture.md §10, relaxed by issue #24 for `claude` specifically
/// (`ClaudeAdapter::env_allowlist`): `HOME` must reach the agent, but an
/// arbitrary marker variable set in the orchestrator's own environment must
/// not -- `env_clear()` still runs first, this is a named allowlist, not a
/// switch to full inheritance.
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
    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
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

/// The run intent must never leak into the agent's argv either -- only
/// `HOME`/`PATH` are allowlisted into the environment, and the intent rides
/// exclusively on stdin (ADR-0012), never as a CLI argument.
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
            "printf '%s' \"$0 $*\" > \"{captures}/coder_argv.txt\"\n{APPEND_NOTES_CODER_BODY}",
            captures = captures.path().display()
        ),
        NOOP_BODY,
        NOOP_BODY,
    );

    let marker = "WARDEN_SECRET_INTENT_MARKER_9f3d21";
    let intent = format!("do the thing ({marker})");

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
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
}

/// Issue #24 point 1, third bullet: the adapter itself transforms `claude`'s
/// `--output-format json` envelope into findings NDJSON
/// (`ClaudeAdapter::extract_findings` -> `warden_core::parse_findings`) --
/// verified end-to-end through the real CLI entry point, with a reviewer
/// that always raises exactly one blocking finding so the run never
/// converges (proving the finding was actually recorded and interpreted,
/// not silently dropped).
#[cfg(unix)]
#[tokio::test]
async fn e2e_reviewer_findings_extracted_through_the_claude_json_envelope_reach_max_cycles() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        r#"printf '%s\n' '{"source":"reviewer","severity":"blocking","description":"always blocking"}' > "$WARDEN_RESULT_FILE""#,
        NOOP_BODY,
    );

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "never converges",
            "--max-cycles",
            "2",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--tool",
            "claude",
        ])
        .assert()
        .success()
        .stdout(contains("finished: MaxCyclesExceeded"));

    let run_id = extract_run_id(&String::from_utf8_lossy(&assert.get_output().stdout));
    let pool = warden::db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::MaxCyclesExceeded);
}

/// A convention file naming a role Claude Code files also use (`tools`) must
/// be passed to the real invocation -- covered at the unit level
/// (`tool_adapter.rs`); this just proves the frontmatter actually reaches
/// the adapter through the full definition-resolution path via the coder's
/// own captured argv.
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

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
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

/// Crash recovery (Architecture.md §9), unaffected by issue #24: a run left
/// in an intermediate state with no live process is marked `Failed` on the
/// next invocation, not resurrected or left stuck.
#[cfg(unix)]
#[tokio::test]
async fn e2e_crashed_run_is_marked_failed_on_the_next_cli_invocation() {
    let warden_home = TempDir::new().unwrap();
    let db_path = warden_home.path().join("state.db");

    // Seed: a run "crashed" mid-cycle, with a dead PID recorded for its
    // coder agent process (deterministic dead pid: spawn-then-wait, not a
    // guessed unused number).
    {
        let pool = warden::db::connect(&db_path).await.unwrap();
        warden::db::insert_run(&pool, "crashed-run", "/tmp/some-repo", "main", "intent", 3)
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
            warden_core::AgentRole::Coder,
            dead_pid,
            "/tmp/wt",
        )
        .await
        .unwrap();
        // Deliberately never mark_agent_process_ended: simulates the
        // orchestrator dying before it could record completion.
        pool.close().await;
    }

    // Restart: a completely unrelated, trivial run against the same
    // --warden-home. Startup crash recovery must run first regardless.
    let repo = init_test_repo();
    let bin_dir = TempDir::new().unwrap();
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
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
