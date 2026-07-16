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
use warden_core::{AgentRole, FindingSource, RunState};

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

/// Stands in for the real `asciinema` binary (Evidence Capture Adapter,
/// ADR-0009): `AsciinemaAdapter` always passes the destination `.cast` path
/// as the last argument (`asciinema rec --quiet --overwrite --command <cmd>
/// <output>`), so this just writes a minimal cast-shaped file there and
/// exits 0. Written into the same fake bin dir as [`write_fake_claude`], so
/// both are on `PATH` together.
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

/// Stands in for `npx --yes playwright test --reporter=list`
/// (`PlaywrightAdapter`): only cares that the command exits 0 and that
/// `test-results/` contains files with a recognized image/video extension
/// afterwards, so this writes exactly that.
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
            "printf '%s' \"$0 $*\" > \"{captures}/coder_argv.txt\"\ncp \"$stdin_file\" \"{captures}/coder_stdin.json\"\n{APPEND_NOTES_CODER_BODY}",
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

    // Positive control (issue #24 review, cycle 2, NIT): without this, the
    // negative assertion above would pass just as happily if the intent were
    // never delivered to the coder at all (e.g. a regression that drops
    // `--intent` on the floor before stdin is ever written) -- a test that
    // can't fail for the right reason. Proves the marker actually arrived
    // over stdin, the one channel it's supposed to use.
    let stdin_dump = std::fs::read_to_string(captures.path().join("coder_stdin.json")).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&stdin_dump).unwrap();
    assert_eq!(payload["intent"], intent);
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

/// Issue #24 point 3, the other half of
/// `e2e_definition_model_and_tools_reach_the_claude_invocation_argv` (which
/// only covers the coder): the convention directory holds up to *three*
/// independent files, `.warden/agents/{coder,reviewer,tester}.md`, and each
/// role's own frontmatter/prompt must reach *that role's own* invocation --
/// never the coder's definition leaking into the reviewer's argv or
/// vice versa.
#[cfg(unix)]
#[tokio::test]
async fn e2e_reviewer_and_tester_definitions_each_reach_their_own_invocation_not_each_others() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    write_agent_definition(repo.path(), "coder", "", "be the coder");
    write_agent_definition(
        repo.path(),
        "reviewer",
        "model: haiku\ntools: Read, Grep\n",
        "be the reviewer",
    );
    write_agent_definition(
        repo.path(),
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

/// Architecture.md §10 (`ClaudeAdapter::env_allowlist`, issue #24 point 6):
/// the allowlist is `["HOME", "USER"]`, not just `HOME` -- the coder's own
/// live-verification note in `tool_adapter.rs` documents `USER` as required
/// for `claude`'s OAuth credential resolution on this platform. This nails
/// that second variable down explicitly, since
/// `e2e_home_reaches_claude_but_other_env_vars_do_not` only asserts `HOME`.
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

    // The test harness's own `USER`, if any, is what should reach the
    // child -- captured here so the assertion isn't hostage to whatever
    // value happens to be set in this particular CI/dev environment.
    let expected_user = std::env::var("USER").unwrap_or_default();

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

// ---------------------------------------------------------------------------
// Ported from the pre-issue-#24 `--coder-agent`/`--reviewer-agent`/
// `--tester-agent` harness (issue #24 review, M3): the underlying behaviour
// these cover is still live and unrelated to the flag removal itself --
// only the harness (fake `claude` on `PATH`, `--tool claude`) changed.
// ---------------------------------------------------------------------------

/// Acceptance criterion 3 (issue #1): the main repo's git history/working
/// tree/branch must be completely untouched by `warden run`, and the
/// converged commit is instead persisted (SQLite) and protected (a
/// `refs/warden/runs/<id>/cycle-<n>` ref in the *main* repo, since the
/// coder's own worktree is removed once the cycle ends).
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

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "single converging cycle",
            "--branch",
            "main",
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
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    // The main repo's current branch/HEAD must be exactly what it was
    // before `warden run` -- writing `refs/warden/...` must never touch
    // `refs/heads/...` or move HEAD.
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

    // No `db.rs` getter exposes a single cycle's row yet, so this reads the
    // column directly -- a test-only convenience, not new production API.
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

    // M4: the commit must be reachable via a local ref in the *main* repo
    // (never the now-removed coder worktree) so it survives `git gc`.
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

/// Acceptance criterion 1 (issue #2): "Aucune collision d'écriture constatée
/// sur un repo de test avec des findings croisés (reviewer et tester
/// modifiant des fichiers différents en simultané)" -- driven through the
/// real `warden run --tool claude` CLI entry point.
///
/// The reviewer writes `review_target.txt`, then (after a deliberate sleep
/// that overlaps with the tester's run) reads back `test_target.txt` from
/// its own worktree; the tester does the mirror image. If reviewer and
/// tester ever shared a worktree/directory (a write collision), the other
/// role's write -- which completes well before the sleep ends -- would
/// already be visible, instead of the untouched original content. This is
/// what distinguishes "isolated worktrees" from "shared worktree"
/// deterministically, without relying on interleaving order.
#[cfg(unix)]
#[tokio::test]
async fn e2e_reviewer_and_tester_modify_different_files_concurrently_without_collision() {
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
sleep 0.3
seen=$(cat test_target.txt)
printf '{"source":"reviewer","severity":"info","description":"review_target=modified-by-reviewer test_target_seen=%s"}\n' "$seen" > "$WARDEN_RESULT_FILE"
"#;
    let tester_body = r#"
echo modified-by-tester > test_target.txt
sleep 0.3
seen=$(cat review_target.txt)
printf '{"source":"tester","severity":"info","description":"test_target=modified-by-tester review_target_seen=%s"}\n' "$seen" > "$WARDEN_RESULT_FILE"
"#;
    write_fake_claude(bin_dir.path(), coder_body, reviewer_body, tester_body);

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "crossed findings, no collision",
            "--branch",
            "main",
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

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();

    // No `db.rs` getter maps a run to its cycles yet, so a direct query is
    // used here rather than adding production API surface just for a test
    // (same convention as the other tests in this file).
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
        .find(|f| f.source == FindingSource::Reviewer)
        .expect("reviewer finding present");
    let tester_finding = findings
        .iter()
        .find(|f| f.source == FindingSource::Tester)
        .expect("tester finding present");

    assert!(
        reviewer_finding
            .description
            .contains("test_target_seen=original-test"),
        "reviewer's worktree must still see the untouched original \
         test_target.txt, not the tester's concurrent write -- got: {}",
        reviewer_finding.description
    );
    assert!(
        tester_finding
            .description
            .contains("review_target_seen=original-review"),
        "tester's worktree must still see the untouched original \
         review_target.txt, not the reviewer's concurrent write -- got: {}",
        tester_finding.description
    );

    // Cross-check at the worktree-path level too: reviewer and tester must
    // have been assigned distinct directories for this cycle.
    let (reviewer_wt, tester_wt): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT reviewer_worktree_path, tester_worktree_path FROM cycles WHERE id = ?",
    )
    .bind(&cycle_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let reviewer_wt = reviewer_wt.expect("reviewer worktree path recorded");
    let tester_wt = tester_wt.expect("tester worktree path recorded");
    assert_ne!(
        reviewer_wt, tester_wt,
        "reviewer and tester must run in distinct worktree directories"
    );
}

/// Issue #6 acceptance criteria, driven end-to-end through the real CLI
/// restart path (`main.rs::run` unconditionally calls `recover_crashed_runs`
/// before starting any new run): "aucun worktree ni process orphelin ne
/// persiste après un cycle de crash + redémarrage". This seeds a single
/// crashed run that left BOTH kinds of orphaned resources behind -- an
/// on-disk worktree whose owning guard was never dropped (a crash is a
/// `SIGKILL`, not a graceful `Drop`), and a genuinely still-running agent
/// process -- then launches a brand-new, unrelated `warden run` against the
/// same `--warden-home` and checks recovery cleaned up both as a side effect
/// of that single startup, with no manual intervention.
#[cfg(unix)]
#[tokio::test]
async fn e2e_crash_restart_leaves_no_orphan_worktree_or_process() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let db_path = warden_home.path().join("state.db");

    // Seed: a run "crashed" mid-cycle, with a real orphaned worktree on disk
    // and a real, still-running orphaned agent process.
    let (worktree_path, mut orphan_child) = {
        let pool = warden::db::connect(&db_path).await.unwrap();

        let worktree_manager = warden::worktree::WorktreeManager::new(
            repo.path(),
            warden_home.path().join("worktrees"),
        )
        .unwrap();
        // Simulates the crash itself: the `Worktree` guard is forgotten
        // instead of dropped or explicitly removed -- exactly what a
        // SIGKILL'd orchestrator would leave behind.
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
            AgentRole::Coder,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        // Two concurrent agent processes recorded for the same cycle, the
        // way reviewer and tester run in parallel (ADR-0003): an earlier one
        // that is genuinely still alive (a real orphan agent process the
        // crashed orchestrator never reaped or killed), and a later, dead
        // one -- recovery decides whether the *run* crashed based on the
        // latest recorded process (`latest_open_agent_process_for_run`), so
        // the dead one must sort after the live one for this run to be
        // recovered as Failed at all.
        let orphan_child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let orphan_pid = orphan_child.id().unwrap();
        warden::db::insert_agent_process(
            &pool,
            "orphan-e2e-live-process",
            "orphan-e2e-cycle",
            AgentRole::Reviewer,
            orphan_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        // Guarantees the dead process's `started_at` sorts strictly after
        // the live one's, so which row is "latest" is deterministic.
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
            AgentRole::Coder,
            dead_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        pool.close().await;
        (worktree_path, orphan_child)
    };

    // Restart: a completely unrelated, trivial run against the same
    // --warden-home. Startup crash recovery must run first regardless.
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
            "--branch",
            "main",
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

    // Behavior 1: the run is recovered as Failed and its orphan worktree no
    // longer exists on disk.
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

    // Behavior 2: the orphan agent process was actually terminated, not
    // merely forgotten about.
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

/// Issue #6 acceptance criterion: an automatic backup of the SQLite database
/// is taken before a pending schema migration is applied, driven through the
/// real CLI entry point rather than calling `db::connect` directly -- every
/// `warden run` invocation opens the db exactly this way on startup
/// (`main.rs::run`). Simulates restarting a pre-existing Warden installation
/// whose schema predates the latest migrations.
#[cfg(unix)]
#[tokio::test]
async fn e2e_restart_backs_up_db_before_applying_pending_migrations_via_cli() {
    let warden_home = TempDir::new().unwrap();
    std::fs::create_dir_all(warden_home.path()).unwrap();
    let db_path = warden_home.path().join("state.db");

    // Simulate an older installation: only the first migration has ever
    // been applied, so the rest are pending on the next `warden run`.
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

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "trigger startup migration",
            "--branch",
            "main",
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

    // The schema must actually have been migrated to current, not just
    // backed up and left stale.
    let pool = warden::db::connect(&db_path).await.unwrap();
    let (run_id,): (String,) = sqlx::query_as("SELECT id FROM runs LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::Converged);
}

/// Re-test cycle (issue #20 review fix, fdcaa4e), H1 intent, driven through
/// the real CLI end-to-end: an agent that never reads stdin at all and
/// exits immediately must not fail the run, even when the stdin payload is
/// large enough (over a typical 64KiB OS pipe buffer) to guarantee the
/// write is still in flight when the agent exits and closes its read end (a
/// genuine broken pipe, not a small payload that just happens to fit unread
/// in the buffer).
///
/// Ported onto the fake-`claude`-on-`PATH` harness (issue #24) with a
/// deliberately *different* shape than [`write_fake_claude`]: that harness's
/// wrapper script always does `cat > "$stdin_file"` first, for every role,
/// specifically so role-specific fragments can inspect the payload -- which
/// would defeat the exact thing this test needs to prove (an invocation that
/// never reads stdin at all). This test's own fake `claude` binary never
/// reads stdin either, for any role, and instead tells the coder invocation
/// apart from the reviewer/tester invocations the only way it can without
/// reading anything: the coder's own worktree doesn't have `notes.txt` yet.
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

    // Comfortably over a typical 64KiB pipe buffer.
    let large_intent = format!("large intent payload: {}", "x".repeat(200_000));

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            &large_intent,
            "--branch",
            "main",
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
}

/// ADR-0012 (issue #20 Scope B), unaffected by issue #24: prior-cycle
/// findings from a reboucle must reach every role's stdin on the *next*
/// cycle -- driven end-to-end through the real CLI, mirroring the
/// orchestrator unit test's `flip_status_coder`/`status_gated_reviewer`
/// fixtures deterministically: cycle 1 leaves `status.txt` "broken"
/// (reviewer blocks), cycle 2 leaves it "fixed" (reviewer passes).
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

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "flip status to fixed via a reboucle",
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

    // The tester gets the exact same prior-findings context as the reviewer
    // (code-standards.md / ADR-0012: both roles are fed identically) --
    // proven independently rather than assumed from the reviewer's payload.
    let tester_cycle2: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("tester_stdin_2.json")).unwrap(),
    )
    .unwrap();
    let tester_cycle2_findings = tester_cycle2["findings"].as_array().unwrap();
    assert_eq!(tester_cycle2_findings.len(), 1);
    assert_eq!(tester_cycle2_findings[0]["description"], "status is broken");

    // A2 (ADR-0013, issue #22): the role that must actually *fix* the
    // findings finally receives them. Cycle 1's coder has nothing to fix;
    // cycle 2's gets exactly the finding that triggered the reboucle -- and
    // still no `target_commit`/`diff`, which it can read from its own
    // worktree.
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

/// ADR-0012, unchanged by issue #24: a non-ASCII, multi-line system prompt
/// (from a `.warden/agents/coder.md` override) and a non-ASCII `--intent`
/// must both reach the agent's stdin JSON payload intact -- exercising the
/// full UTF-8 round trip through the frontmatter parser, the definition
/// resolver, and the stdin write, not just ASCII fixtures.
#[cfg(unix)]
#[tokio::test]
async fn e2e_non_ascii_multiline_prompt_and_intent_survive_the_stdin_round_trip() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    // Multi-line, accented, emoji, quotes, and a tab: the body is the prompt.
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

    Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
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

// ---------------------------------------------------------------------
// Issue #7 (ADR-0009): Evidence Capture Adapter, ported onto the fake-
// `claude`-on-`PATH` harness (issue #24 review, M3): `--evidence-tool`/
// `--evidence-store-in-repo` are still live CLI flags -- review finding M1
// (an unescaped, tool-adapter-authored prompt breaking `asciinema rec
// --command`) is exactly the class of bug this coverage exists to catch.
// ---------------------------------------------------------------------

/// Acceptance criterion 1 (issue #7, CLI direction): a project with no web
/// markers is classified `Cli`, selecting asciinema. Also covers criterion 3
/// (`evidence.store_in_repo` defaults to `true`, since
/// `--evidence-store-in-repo` is deliberately omitted here) and criterion 4
/// (the captured artifact is committed under `.warden/evidence/<cycle>/` in
/// a dedicated commit on top of the coder's own commit, only at
/// convergence) and criterion 5 (the `EVIDENCE` row round-trips through
/// SQLite) -- all driven through the real `warden run --tool claude` CLI
/// entry point.
#[cfg(unix)]
#[tokio::test]
async fn e2e_cli_project_selects_asciinema_and_evidence_is_stored_and_committed_by_default() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    // No package.json, no web marker files anywhere in the repo -> Cli.
    write_fake_claude(
        bin_dir.path(),
        APPEND_NOTES_CODER_BODY,
        NOOP_BODY,
        NOOP_BODY,
    );
    write_fake_asciinema(bin_dir.path());

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "cli project captures evidence via asciinema",
            "--branch",
            "main",
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

    // store_in_repo defaults to true (--evidence-store-in-repo omitted):
    // the artifact must be committed under .warden/evidence/<cycle>/, in a
    // dedicated commit layered on top of the coder's own commit.
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

    // Never merged/checked out into the user's own working tree.
    let status = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(status.stdout.is_empty());
}

/// Acceptance criterion 1 (issue #7, web direction): a project with a known
/// web marker file (`index.html`, present in the tester's own worktree
/// since it's checked out at the coder's commit) is classified `Web`,
/// selecting Playwright -- the mirror image of the asciinema/Cli test above.
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

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "web project captures evidence via playwright",
            "--branch",
            "main",
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

/// Acceptance criterion 2: `evidence.tool` config override always wins over
/// auto-detection. The repo carries an unambiguous web marker
/// (`index.html`) -- auto-detection alone would select Playwright -- but
/// `--evidence-tool asciinema` must still force the asciinema adapter,
/// observable by the artifact's file name (`session.cast`, which only the
/// asciinema adapter ever produces).
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

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "override forces asciinema on a web-looking project",
            "--branch",
            "main",
            "--max-cycles",
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

/// Acceptance criteria 3/4: `evidence.store_in_repo` can be turned off
/// (`--evidence-store-in-repo false`), in which case a captured artifact
/// stays on local scratch storage only -- never committed into the repo,
/// and the converged commit stays exactly the coder's own commit.
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

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", path_with_fake_bin_first(bin_dir.path()))
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "evidence stays local when store-in-repo is disabled",
            "--branch",
            "main",
            "--max-cycles",
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

    // Still captured locally, regardless of store_in_repo.
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

    // Never committed into the repo: no evidence ref exists, and the
    // converged commit is exactly the coder's own commit.
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

/// Acceptance criterion 7: "a missing/failing evidence tool is non-fatal --
/// a converging run still converges", driven end-to-end through the real
/// CLI with genuinely no `asciinema`/Playwright tooling on `PATH` beyond the
/// fake `claude` (this sandbox has neither installed for real either).
#[cfg(unix)]
#[tokio::test]
async fn e2e_evidence_capture_failure_when_tool_missing_is_non_fatal_and_run_still_converges() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    // Only `claude` is on this fake bin dir -- deliberately no
    // `asciinema`/`npx` stand-in, so the (Cli -> asciinema) capture attempt
    // fails on an ordinary missing-binary error.
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
            "converges even though no evidence tool is installed",
            "--branch",
            "main",
            "--max-cycles",
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
