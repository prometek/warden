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
use predicates::prelude::*;
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

/// Writes `<xdg_config_home>/warden/agents/<role>.md` -- the reviewer/
/// tester's own trusted source since issue #26
/// (`agent_def::default_user_config_agents_dir`). Distinct from
/// `write_agent_definition` (the *repo's* `.warden/agents/` convention,
/// reviewer/tester-ignored-by-default since the same issue): same file
/// shape, a completely different root the coder has no write access to.
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

/// Issue #26 review, LOW: every `assert_cmd::Command` driving the real
/// `warden` binary in this file goes through this helper rather than
/// `Command::cargo_bin("warden")` directly -- this fn's own body is the only
/// remaining call to `Command::cargo_bin("warden")` in this file (not an
/// enforced invariant, just an accurate statement of the current file --
/// nothing stops a future test from bypassing this helper, so keep this
/// claim honest rather than re-asserting the "impossible to reintroduce by
/// omission" language a prior review found unenforced). `assert_cmd`'s
/// `Command::env` only *adds* to the inherited environment, it never clears
/// it -- a test
/// that forgets to set `XDG_CONFIG_HOME`/`HOME` itself would otherwise
/// silently fall through to whatever the real developer/CI environment
/// happens to contain (code-standards.md: tests must never touch the real
/// `~/.config` or real `$HOME`; this was a real bug -- see
/// `e2e_non_git_repo_path_is_a_clean_cli_error`, which gets far enough into
/// `main.rs` to actually read `~/.config/warden/agents/{reviewer,tester}.md`
/// before its own assertion is even reached). A fresh, empty, hermetic
/// directory is set as both unconditionally, and a test that needs its own
/// fake `claude` on `PATH` (or its own explicit `XDG_CONFIG_HOME`/`HOME`)
/// still overrides it with a later `.env(...)`/`.env_remove(...)` call --
/// later calls win under `assert_cmd`.
///
/// **Two sites in this file cannot go through this helper** and use
/// `std::process::Command`/`SyncCommand` directly instead
/// (`e2e_run_survives_a_closed_stdout_without_panicking`,
/// `e2e_tui_flag_does_not_block_on_a_still_running_tui_when_stdout_is_not_a_terminal`):
/// both need `.stdout(Stdio::piped())`/`.spawn()`/`Child::wait()` to observe
/// the child process while it is still running, which `assert_cmd::Command`
/// (this helper's own return type) does not expose at all -- it only offers
/// `.output()`/`.unwrap()`/`.assert()`, which run the child to completion
/// internally. Both still set `XDG_CONFIG_HOME` explicitly on their own raw
/// `Command`, so neither leaks into the real `~/.config`.
///
/// Returns the hermetic `TempDir` alongside the `Command` -- it must be kept
/// alive by the caller for as long as the `Command` might still be read
/// (through to `.assert()`/`.output()`), since dropping a `TempDir` deletes
/// the directory it points at.
fn warden_command() -> (Command, TempDir) {
    let hermetic_home = TempDir::new().expect("tempdir");
    let mut cmd = Command::cargo_bin("warden").unwrap();
    cmd.env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("XDG_CONFIG_HOME", hermetic_home.path())
        .env("HOME", hermetic_home.path());
    (cmd, hermetic_home)
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

/// Issue #31: `warden run`, invoked exactly as a user would (no `-v`,
/// default `warn` verbosity), must print the run id and a ready-to-copy
/// `warden-tui attach` command to **stdout** at the **start** of the run --
/// not only once it finishes. Driven through the real compiled binary, the
/// same entry point a human or CI caller uses.
///
/// Covers, in one pass through the real CLI:
/// - the two new lines are present verbatim, with no `-v` flag;
/// - they appear *before* the pre-existing `run {id} finished: {state}`
///   line (ordering is the whole point of the issue);
/// - the run id in the "started" line, the "attach:" line, and the
///   "finished" line are all the exact same id, and that id matches the
///   `runs` row actually persisted in SQLite;
/// - the printed `--warden-home` is the exact effective value the run used
///   (here: the explicit flag, verbatim -- the "resolved instead of unset"
///   case is covered separately below).
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
        // Deliberately no `-v`/`-vv`/`-vvv`: the default verbosity is
        // `warn`, and these lines must be visible there without the caller
        // opting into any extra logging.
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

    // Cross-checked against the persisted `runs` row, not just stdout's own
    // internal consistency: proves the printed id is really this run's,
    // not e.g. a stale/hardcoded value that happens to look right.
    let pool = warden::db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let run = warden::db::get_run(&pool, started_run_id)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no `runs` row found for id {started_run_id}"));
    assert_eq!(run.id, started_run_id);
}

/// Issue #31: "reprendre le `--warden-home` effectif (résolu, pas la valeur
/// brute du flag)". When `--warden-home` is omitted, `warden` falls back to
/// `$HOME/.warden` (`default_warden_home`) -- the printed attach command
/// must reflect that *resolved* default, not an empty/unset placeholder, so
/// it is still copy-pasteable verbatim.
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
            // `--warden-home` deliberately omitted.
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

    // The resolved db path must actually exist there too -- the printed
    // path isn't just cosmetically plausible, it's where this run's own
    // state genuinely landed.
    assert!(
        expected_resolved_home.join("state.db").exists(),
        "the resolved default warden-home ({}) must be where this run's state.db actually is",
        expected_resolved_home.display()
    );
}

/// Issue #31 review, M1: a `--warden-home` containing a space must still
/// produce a paste-safe `attach:` line, shell-quoted rather than
/// interpolated raw -- the exact bug the review reproduced against the real
/// binary (an unquoted path like `.../My Drive` feeds `warden-tui attach` a
/// stray `Drive` argument once pasted). Verified by shell-splitting the
/// printed line back into argv (`shlex::split`, the same crate the fix
/// itself uses) and asserting the space-containing warden_home survives as
/// exactly one argument, not two.
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

/// Issue #31 review, M3: a RELATIVE `--warden-home` must be printed as an
/// ABSOLUTE path in the attach command, so it stays paste-safe even from a
/// different cwd than the one `warden run` itself was invoked from -- the
/// "resolved, not raw flag value" requirement applies just as much to
/// relativeness as it does to the default-vs-explicit case already covered
/// above. Verified by invoking the binary with a relative `--warden-home`
/// from a controlled `current_dir`, and asserting the printed path equals
/// that cwd joined with the relative value -- not the relative value
/// itself.
///
/// Deliberately does NOT assert the run itself reaches `Converged`: a
/// relative `--warden-home` hits a separate, pre-existing bug unrelated to
/// issue #31 -- `WorktreeManager::create` (`worktree.rs`) resolves it via
/// `git -C <main_repo_path> worktree add <relative_worktrees_root>/...`
/// (relative to the *repo*), while `read_head_commit` (`orchestrator.rs`)
/// later does `git -C <relative_worktrees_root>/...` with no `-C` override
/// at all (relative to the *process cwd*) -- two different git invocations
/// resolving the exact same relative string against two different bases.
/// The printed attach line (this test's actual subject) is written before
/// either of those git calls ever runs, so it is unaffected; only the run's
/// own eventual convergence is. Confirmed independently: `state.db` (opened
/// by `db::connect`, resolved the same way SQLite resolves any relative
/// path -- against the process cwd) does land exactly where the printed
/// line says it does.
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
    // Not `.success()`: see this test's own docs for the unrelated,
    // pre-existing relative-path git worktree bug this run is expected to
    // hit downstream of the print statement under test here.
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

    // The relative value itself must never appear as the `--warden-home`
    // argument of the printed command -- only its absolutized form should.
    assert!(
        !stdout.contains(&format!("--warden-home {relative_warden_home}")),
        "the attach command must not echo the raw relative --warden-home value verbatim: \
         {stdout:?}"
    );

    // `cwd_root.path()` itself may still contain a symlinked component
    // (e.g. macOS's `/var` -> `/private/var`); `std::path::absolute` is
    // purely lexical and does not resolve those, but `getcwd(2)` -- what
    // the child process's own `std::env::current_dir()` call reports, once
    // it has actually `chdir`'d there -- always returns the fully resolved
    // path. Canonicalizing here (the tempdir already exists, so this cannot
    // fail) matches that, rather than comparing against the symlinked form.
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

    // Closes the loop: the resolved absolute path is really where this
    // run's own state landed, not just a cosmetically-plausible string.
    assert!(
        expected_absolute.join("state.db").exists(),
        "the absolutized relative warden-home ({}) must be where this run's state.db actually \
         is",
        expected_absolute.display()
    );
}

/// Issue #31 review, L2: `warden run | head -1` must not panic on a closed
/// stdout, and the run it started must still reach a terminal state
/// (`Converged`) in SQLite rather than being aborted mid-flight with its
/// `runs` row stuck non-terminal. Reproduced against the real compiled
/// binary without a shell (so the test controls exactly when the read end
/// closes, deterministically, rather than racing an external `head`
/// process): the child's stdout is read one line, then the read end is
/// dropped -- closing the pipe -- strictly before `Child::wait()` is called,
/// so every later write (in particular the end-of-run `finished:` line,
/// which is only ever written after the whole convergence loop -- and thus
/// every write this test cares about -- has completed) is guaranteed to
/// race against an already-closed pipe, not a possibly-still-open one.
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

    // Drained concurrently on its own thread so a chatty stderr can't fill
    // its pipe buffer and deadlock `child.wait()` below.
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
        // Dropping `reader` (and the `ChildStdout` it owns) here closes our
        // read end of the pipe -- this happens strictly before `child.wait()`
        // below, in program order, so there is no race: every write the
        // child performs afterwards (in particular the end-of-run line)
        // necessarily targets an already-closed pipe.
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

/// M2 (issue #20 review): `--intent ""` must be a clean CLI parse error,
/// never a run that gets far enough to write a `runs` row before failing.
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

/// `worktree::WorktreeManager::new` rejects a `--repo` with no `.git` --
/// this must surface as a clean CLI failure, not a panic.
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

/// Issue #24 point 1: `--tool` is validated against a closed, compiled-in
/// set at the CLI boundary (code-standards.md: "valider toute entrée
/// externe... à la frontière") -- an unsupported value is a clean parse
/// error naming what was given, never silently defaulted to whatever
/// adapter happens to exist.
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

/// `--tool` has no default: the target UX (issue #24) shows it explicitly on
/// every example invocation, so omitting it entirely must be a clean parse
/// error, not a silent fallback to whichever adapter happens to be first.
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

/// Issue #49: `--isolation` is validated against a closed, compiled-in set
/// at the CLI boundary (code-standards.md: "valider toute entrée externe...
/// à la frontière"), the exact same pattern `--tool` already uses -- an
/// unsupported value is a clean parse error naming what was given, never
/// silently defaulted to `worktree`.
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

/// Issue #49: `--isolation` defaults to `worktree` when omitted entirely --
/// unlike `--tool` (which has no default, issue #24), omitting `--isolation`
/// must not itself fail arg parsing; it is exercised indirectly by every
/// other `e2e_*` test in this file that never passes `--isolation` at all
/// and still runs a real (non-docker) convergence loop successfully.
#[test]
fn e2e_omitting_isolation_entirely_defaults_to_worktree_not_a_cli_error() {
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
    ])
    .assert()
    .failure()
    // Fails downstream (no fake `claude` on `PATH`), never on arg parsing --
    // proven by never seeing clap's own "--isolation" complaint.
    .stderr(contains("--isolation").not());
}

/// Cheapest daemon-reachability probe available (mirrors
/// `warden_sandbox::docker`'s own test-only `docker_daemon_available`) --
/// auto-skips the behavioural test below rather than failing the whole
/// suite on a machine without Docker installed/running.
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

/// Resolves the docker endpoint this test's own (real) environment actually
/// uses, so it can be forwarded explicitly to `warden_command()`'s hermetic
/// `HOME` below. Docker's own CLI resolves its context (and therefore which
/// socket to dial) from `$HOME/.docker/config.json` when `$DOCKER_HOST` is
/// unset -- many real setups (Docker Desktop's `desktop-linux` context, in
/// particular) point at a non-default socket path under the real `$HOME`,
/// not the standard `/var/run/docker.sock`. Overriding `HOME` for a hermetic
/// run (this file's own established, load-bearing convention -- see
/// `warden_command`'s own docs) would otherwise make every `docker` child
/// process `warden` spawns dial the wrong (or a nonexistent) socket, even
/// though this test's own preceding `docker_daemon_available`/`docker build`
/// calls -- which inherit the real environment untouched -- succeed just
/// fine. Explicit `$DOCKER_HOST` bypasses context resolution entirely,
/// working regardless of the current machine's docker context setup.
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

/// Issue #49 acceptance criterion 1, closing a real coverage gap: every
/// other `--isolation docker` test in this file (above) only exercises the
/// CLI *parse* half (`--isolation docker` is accepted / rejected as a
/// string) -- none of them actually drive a `warden run --isolation docker`
/// invocation through to a real container. This one does: it builds a
/// throwaway image whose `claude` is a fake script that (a) hard-fails
/// unless `/.dockerenv` exists, so the run can only converge if the coder
/// invocation genuinely executed inside a container, and (b) is the *only*
/// `claude` anywhere reachable -- deliberately not placed on this test's own
/// host `PATH` -- so if `--isolation docker` were silently ignored (falling
/// back to `LocalSandbox`, which resolves `claude` via the host `PATH` this
/// process inherits), the run would fail to spawn it at all rather than
/// quietly succeeding on the host. The coder additionally drops a proof file
/// directly into the base repo's *common* `.git` directory (bind-mounted
/// read-write, and, unlike the role's own ephemeral worktree, never cleaned
/// up after the run) -- read back from the host afterwards as positive
/// evidence a container actually ran, not just an absence-of-failure
/// argument.
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

    // `--isolation docker` resolves `~/.claude` from `HOME` (see
    // `default_claude_config_dir`) -- `warden_command()`'s own hermetic
    // `HOME` needs that directory to actually exist for `DockerSandbox` to
    // canonicalize it, exactly like `init_repo_with_worktree_and_claude_dir`
    // in `warden-sandbox`'s own tests.
    let claude_dir = hermetic_home.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join(".credentials.json"), "{}").unwrap();

    // Build a throwaway image: plain `alpine` (already used by
    // `warden-sandbox`'s own docker tests) plus `git` and the fake `claude`
    // script below. Tagged with a fresh uuid so parallel test runs (and
    // repeated local runs) never collide on the same tag.
    let image_tag = format!("warden-cli-test-{}", uuid::Uuid::new_v4());
    let build_dir = TempDir::new().unwrap();
    std::fs::write(
        build_dir.path().join("Dockerfile"),
        "FROM alpine:latest\n\
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

    // Deliberately no fake `claude` placed on this test's own `PATH` -- see
    // this test's own docs on why that absence is itself part of the
    // assertion.
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

/// A convention file that exists but fails to read for a reason other than
/// "doesn't exist" (here: it's a directory) must be a clean CLI error, not a
/// silent fallback to the adapter's default prompt.
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
        .stdout(contains("finished: MaxReviewCyclesExceeded"));

    let run_id = extract_run_id(&String::from_utf8_lossy(&assert.get_output().stdout));
    let pool = warden::db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::MaxReviewCyclesExceeded);
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

/// Issue #24 point 3, the other half of
/// `e2e_definition_model_and_tools_reach_the_claude_invocation_argv` (which
/// only covers the coder): the coder's own convention file
/// (`.warden/agents/coder.md`) and the reviewer/tester's own trusted source
/// since issue #26 (`$XDG_CONFIG_HOME/warden/agents/{reviewer,tester}.md`)
/// are three independent files, and each role's own frontmatter/prompt must
/// reach *that role's own* invocation -- never the coder's definition
/// leaking into the reviewer's argv or vice versa.
#[cfg(unix)]
#[tokio::test]
async fn e2e_reviewer_and_tester_definitions_each_reach_their_own_invocation_not_each_others() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();
    // Issue #26: reviewer/tester definitions are resolved from the user
    // config directory, not the repo under review -- a dedicated tempdir
    // here (rather than reusing `warden_home`) both keeps this test hermetic
    // (never touches the real `~/.config`) and exercises the trusted source
    // this issue actually added.
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

/// Issue #26 review, MEDIUM: with `--trust-repo-agents` off (the default)
/// and no user-config file, a repo-supplied `.warden/agents/reviewer.md`
/// must be ignored -- the reviewer runs with the adapter's own default
/// prompt, never the repo's, and the CLI's own stderr names the ignored path
/// (this is the single most important assertion issue #26 exists to pin:
/// nothing in `main.rs`'s wiring or `agent_def::resolve_agent_definition`
/// accidentally lets a repo-controlled prompt reach an independent role by
/// default).
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
            // Deliberately no `--trust-repo-agents`.
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    // The `tracing::warn!` this run emits is checked across both streams
    // rather than assuming one specific stream: `init_tracing`'s own
    // `tracing_subscriber::fmt()` builder uses its crate default writer
    // (stdout) here, distinct from `main.rs`'s own deliberately-`stderr`-
    // agnostic structured "run started"/"finished" lines -- this assertion
    // only cares that the warning was emitted somewhere in this process's
    // own output, not which stream carried it.
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

/// Issue #26 review, MEDIUM: the opt-in escape hatch, driven end-to-end --
/// `--trust-repo-agents` makes the repo's own `.warden/agents/reviewer.md`
/// actually reach the reviewer's invocation, with the CLI naming the path as
/// untrusted on stderr *and* persisting a
/// `RunEvent::UntrustedAgentDefinitionUsed` for it, so both this process's
/// own log and the run's own permanent, replayable event log carry the
/// record (see `agent_def`'s own module docs on why the flag must never be
/// silently indistinguishable from a trusted resolution).
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
        events.iter().any(|record| matches!(
            &record.event,
            warden_core::RunEvent::UntrustedAgentDefinitionUsed { role, path, canonical_path }
                if role == "reviewer"
                    && path == &expected_path.display().to_string()
                    && canonical_path == &expected_canonical_path.display().to_string()
        )),
        "expected an UntrustedAgentDefinitionUsed event for the reviewer naming {}: {events:?}",
        expected_path.display()
    );
}

/// Issue #26 review, HIGH: a `user_config_agents_dir` that itself resolves
/// *inside* the repo under review (the coder-controlled-`XDG_CONFIG_HOME`
/// attack this fix closes -- e.g. a committed `.envrc` exporting
/// `XDG_CONFIG_HOME=$PWD/.config`) must never be treated as the trusted
/// `AgentDefinitionSource::UserConfig` -- with the flag off, it is ignored
/// (with the same warning); with the flag on, it is used but surfaced as
/// untrusted exactly like a repo convention file, never silently accepted as
/// the genuinely trusted source.
#[cfg(unix)]
#[tokio::test]
async fn e2e_xdg_config_home_pointing_inside_the_repo_is_degraded_to_untrusted() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();
    // The attack: `XDG_CONFIG_HOME` resolves to a directory *inside* the
    // repo the coder controls, exactly as a committed `.envrc` picked up by
    // the invoking shell would produce.
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

    // Flag off: degraded source is ignored, exactly like a repo convention
    // file.
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
        // Issue #26 review, LOW: the degraded-user-config case gets its own
        // distinct warning text, not the plain repo-convention-file one --
        // "move it to $XDG_CONFIG_HOME/warden/agents/" would be a no-op here
        // since the file already lives there.
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

    // Flag on: degraded source is used, but surfaced as untrusted.
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
        // Issue #26 review, LOW: the persisted event must carry the literal
        // `XDG_CONFIG_HOME`-relative path an operator recognizes *and* the
        // canonical path proving it actually resolves inside the repo --
        // here they agree (no symlink involved, just an `XDG_CONFIG_HOME`
        // pointed straight at a repo-inside directory), but both fields must
        // still be present and correct.
        let expected_canonical_path = expected_path.canonicalize().unwrap();
        assert!(
            events.iter().any(|record| matches!(
                &record.event,
                warden_core::RunEvent::UntrustedAgentDefinitionUsed { role, path, canonical_path }
                    if role == "reviewer"
                        && path == &expected_path.display().to_string()
                        && canonical_path == &expected_canonical_path.display().to_string()
            )),
            "{events:?}"
        );
    }
}

/// Issue #26 review, MEDIUM: `default_user_config_agents_dir`'s `HOME`-only
/// fallback (`$HOME/.config/warden/agents`) -- an explicit ticket
/// requirement -- exercised with `XDG_CONFIG_HOME` genuinely unset (not just
/// empty), so the fallback branch is really what resolves the reviewer's
/// trusted source, not `XDG_CONFIG_HOME` happening to agree with it.
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

/// The same fallback, exercised via the *other* documented trigger: a
/// blank/whitespace-only `XDG_CONFIG_HOME` (set, but not usable) must fall
/// back to `$HOME/.config` exactly like an unset one --
/// `default_user_config_agents_dir`'s own `.trim().is_empty()` branch.
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

/// `default_user_config_agents_dir`'s first `UserConfigDirUnresolvable`
/// branch: neither `XDG_CONFIG_HOME` nor `HOME` set at all.
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

/// `default_user_config_agents_dir`'s second `UserConfigDirUnresolvable`
/// branch: `HOME` is set but empty, with `XDG_CONFIG_HOME` unset too.
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
        warden::db::insert_run(
            &pool,
            "crashed-run",
            "/tmp/some-repo",
            "main",
            "intent",
            3,
            3,
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
/// modifiant des fichiers différents)" -- driven through the real
/// `warden run --tool claude` CLI entry point.
///
/// Reviewer and tester no longer run concurrently (issue #40, ADR-0003
/// amendment: `run_review` then `run_test`, sequentially), so this no longer
/// needs the deliberate overlapping sleep the original (issue #2, parallel
/// `tokio::join!`) version of this test relied on -- worktree isolation is
/// exercised the same way regardless of timing. The reviewer writes
/// `review_target.txt`, then reads back `test_target.txt` from its own
/// worktree; the tester does the mirror image. If reviewer and tester ever
/// shared a worktree/directory (a write collision), the other role's write
/// would already be visible, instead of the untouched original content --
/// this is what distinguishes "isolated worktrees" from "shared worktree".
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

        // Two agent processes recorded for the same cycle -- the
        // `agent_processes` schema has always allowed more than one row per
        // cycle (originally because reviewer and tester ran in parallel,
        // ADR-0003; unaffected by issue #40 moving them to sequential, since
        // this is purely about what recovery does with whatever rows it
        // finds): an earlier one that is genuinely still alive (a real
        // orphan agent process the crashed orchestrator never reaped or
        // killed), and a later, dead one -- recovery decides whether the
        // *run* crashed based on the latest recorded process
        // (`latest_open_agent_process_for_run`), so the dead one must sort
        // after the live one for this run to be recovered as Failed at all.
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

    // Comfortably over a typical 64KiB pipe buffer, yet under Linux's
    // per-argument limit (MAX_ARG_STRLEN = 128KiB): the intent travels as a
    // single `--intent` argv entry, and a longer string fails with E2BIG
    // ("Argument list too long") on Linux while silently passing on macOS.
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

/// ADR-0012 (issue #20 Scope B), unaffected by issue #24: prior-cycle
/// findings from a reboucle must reach every role's stdin on the *next*
/// cycle -- driven end-to-end through the real CLI, mirroring the
/// orchestrator unit test's `flip_status_coder`/`status_gated_reviewer`
/// fixtures deterministically: cycle 1 leaves `status.txt` "broken"
/// (reviewer blocks), cycle 2 leaves it "fixed" (reviewer passes).
///
/// Issue #41, ADR-0014 (Phase A -- gate review): the tester never runs
/// during cycle 1, since the reviewer blocks it -- its one and only
/// invocation lands in cycle 2, once the review gate opens, so it is the
/// tester's *first* capture (`tester_stdin_1.json`), not its second.
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

    // The tester gets the exact same prior-findings context as the reviewer
    // (code-standards.md / ADR-0012: both roles are fed identically) --
    // proven independently rather than assumed from the reviewer's payload.
    // Issue #41 (Phase A gate review): the tester never runs in cycle 1 (the
    // reviewer is still blocking), so its one capture is its *first*
    // invocation overall, named `tester_stdin_1.json` by the fixture's own
    // invocation counter -- not `tester_stdin_2.json`.
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

/// Issue #32: `--tui --tui-bin <path>` must spawn that exact binary with
/// `attach --run-id <id> --warden-home <warden_home>` -- the same argv a
/// user would type by hand from the printed "attach:" hint (issue #31).
///
/// The fake `warden-tui` sleeps well past this test's own runtime before
/// exiting -- if it exited (correctly) instantly instead, it would race the
/// (fast, fake-claude) convergence loop and cancel the run before it ever
/// converges (the decided "TUI exit cancels the run" behaviour, exercised on
/// its own by `e2e_tui_exit_cancels_a_still_running_run` below), which is not
/// what this test is about.
///
/// The sleep is 30s, not a duration merely *expected* to outlast convergence.
/// It was 1s, then 3s: issue #30's `AgentDefinitionSnapshot` re-resolution
/// added real `git worktree` subprocess work to every cycle, which widened
/// the race under `cargo test --workspace`'s full parallel load, and the
/// bump to 3s bought margin while leaving "a real race in principle". It
/// still lost that race on the issue #50 branch, failing with `process for
/// \`claude\` was cancelled`. 30s stops picking a number that convergence is
/// merely expected to beat.
///
/// The fake closes its inherited stdout/stderr (`exec 1>&- 2>&-`) *before*
/// sleeping, which is what makes the long sleep free. `spawn_tui_attach`
/// inherits stdio, so a fake that just slept would hold its own copy of
/// `warden run`'s stdout pipe open for the full 30s, and `.assert()` -- which
/// reads that pipe to EOF -- would wait on the fake TUI rather than on
/// `warden run` (the hazard spelled out in
/// `e2e_tui_flag_does_not_block_on_a_still_running_tui_when_stdout_is_not_a_terminal`'s
/// own docs, which sidesteps it with `Child::wait()` instead). Closing the
/// descriptors models what a real `warden-tui` does anyway -- it releases its
/// inherited copy promptly once its Event Bus connection closes -- so this
/// test asserts on `warden run`'s own output at `warden run`'s own pace,
/// while the fake stays alive long enough to never cancel the run.
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

    // `warden run` waits for the spawned `warden-tui` to exit before
    // returning (see the dedicated lifecycle test below), so by the time
    // `.assert()` above has returned, the fake script has already run to
    // completion and its argv capture file is guaranteed to be there --
    // no polling needed here, unlike that other test's longer-lived fake.
    let captured = std::fs::read_to_string(&captured_argv).unwrap();

    // `printf '%s\n' "$@"` (rather than `"$*"`) prints one argv entry per
    // line -- unambiguous even if an argument itself contained a space,
    // unlike joining on `$*`'s `IFS`.
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

/// Issue #32 re-review: on a non-tty stdout -- exactly what a real `warden
/// run --tui > file` / `| tee log` / CI invocation gets -- `warden run`'s
/// own process must exit promptly once the run converges, without waiting
/// for a still-alive `warden-tui` first. Waiting unconditionally (this
/// issue's first re-review fix) deadlocked for real on this exact path: a
/// non-tty `warden-tui attach` runs `run_headless`, which only self-exits
/// once its live channel closes, which only happens once this very
/// process's `EventBus` (and the `broadcast::Sender` it owns) is dropped --
/// which only happens once `run` returns. `should_wait_for_spawned_tui`
/// gates on stdout's tty-ness precisely to keep this scriptable/headless
/// path (still documented for e.g. `warden run --tui > events.ndjson`)
/// working exactly as it did before `--tui` existed.
///
/// Deliberately measures `Child::wait()` directly -- the moment `warden
/// run`'s own process actually exits -- rather than reading its stdout to
/// EOF (what `assert_cmd`/`Command::output` do internally): the fake
/// `warden-tui` here sleeps well past this test's own bound to model "still
/// alive", and since `spawn_tui_attach` inherits stdio, that still-sleeping
/// process also holds its own copy of `warden run`'s stdout pipe open for
/// as long as it stays alive. A reader waiting for that pipe to reach EOF
/// (as `.output()` does) would then wait on *the fake TUI*, not on `warden
/// run` -- which is a real, separate, and already-understood hazard of
/// piping a `--tui` run's own output (see `should_wait_for_spawned_tui`'s
/// own docs), but an artifact this test isn't about: a real `warden-tui`
/// closes its own inherited copy promptly, once it notices its Event Bus
/// connection close, unlike a fake stand-in that just sleeps
/// unconditionally. `Child::wait()` sidesteps that artifact entirely and
/// directly answers the one question this test asks: did `warden run`
/// itself return without waiting for the TUI.
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
    // Sleeps far longer than this test's own bound below -- models "the TUI
    // is still alive/has not exited on its own". Bare `sleep`, not
    // reading stdin: a null stdin (below) already reads as EOF immediately,
    // so a fake TUI that tried to block on reading it wouldn't actually
    // stay alive at all -- see this test's own docs for why sleeping,
    // rather than something tied to stdin, is what models "still running"
    // here.
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

    // Drained on background threads (never joined here) purely so `warden
    // run`'s own writes never block on a full OS pipe buffer -- not used
    // for this test's assertions, see this test's own docs on why reading
    // either pipe to EOF is exactly the artifact this test avoids.
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

/// Issue #32 decision ("la sortie de la TUI annule le run"): once the TUI
/// exits -- for any reason, here simply by returning immediately -- the
/// still-running run must be cancelled rather than left to run to
/// completion. Driven with a coder that sleeps far longer than this test's
/// own bound, so a passing assertion on wall-clock time only holds if the
/// run was genuinely cancelled early, not if it happened to finish quickly
/// on its own.
#[cfg(unix)]
#[tokio::test]
async fn e2e_tui_exit_cancels_a_still_running_run() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    // The coder sleeps far longer than the bound asserted below -- if
    // cancellation didn't fire, this test would time out rather than fail
    // fast, making a regression here impossible to miss.
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

/// Issue #32 review (MEDIUM): a `--tui` spawn failure must abort the run
/// with its own actionable error, not silently degrade to a plain headless
/// run -- `--tui` is an explicit user request for the spawned TUI and the
/// cancel-on-exit safety net it provides, and code-standards.md forbids
/// exactly this kind of silent fallback. Driven with a `--tui-bin` that
/// does not exist at all, so `spawn_tui_attach` fails immediately with a
/// typed `ProcessError::Spawn`.
#[cfg(unix)]
#[tokio::test]
async fn e2e_tui_spawn_failure_aborts_the_run_instead_of_degrading_to_headless() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    // The coder sleeps well past this test's own assertions below -- if the
    // run were left running headless instead of aborting, this would show
    // up as a slow/hanging test rather than a passing one.
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

// ---------------------------------------------------------------------
// Issue #72: multi-intent batch mode.
// ---------------------------------------------------------------------

/// Every `"run <id> finished: <State>"` line in `stdout`, in order -- the
/// batch equivalent of [`extract_run_id`] (which only ever expects one).
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

/// Issue #72 acceptance criterion: "3 intents provided -> processed one
/// after another, each to completion". Reuses the exact coder/reviewer/
/// tester fixture the single-intent "converges cleanly" tests already use
/// (`APPEND_NOTES_CODER_BODY`/`NOOP_BODY`/`NOOP_BODY`) -- proving batch mode
/// converges each intent is the same claim those tests make, just repeated
/// three times through one `warden run` invocation with three `--intent`
/// flags instead of three separate invocations.
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

    // Issue #72: "between two intents ... no shared state" -- each of the 3
    // intents got its own distinct run_id and its own `runs` row.
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

/// Issue #72 acceptance criterion: "a failing intent doesn't block the
/// following ones (continue by default)". The reviewer here only ever
/// blocks when the incoming payload's `intent` field carries a specific
/// marker (checked against `$stdin_file`, exactly like
/// `e2e_run_intent_never_leaks_into_the_coders_argv`'s existing use of
/// `$stdin_file` for a reviewer/tester-observable marker) -- so exactly the
/// middle of 3 intents is forced into `MaxReviewCyclesExceeded` while the
/// other two converge normally, proving the batch actually continued past
/// it rather than stopping.
#[cfg(unix)]
#[tokio::test]
async fn e2e_batch_continues_past_a_non_converged_intent_by_default() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    // Issue #20 Scope B / A2 (ADR-0012): only the coder's own payload
    // carries the run `intent` -- the reviewer/tester payload never does
    // (`AgentInputMessage::for_finding_agent` has no `intent` field at all).
    // So this coder fragment writes the intent it *did* receive into the
    // file it commits, making it observable to the reviewer the only way a
    // reviewer ever sees anything about a cycle: through the coder's own
    // diff (also part of its own `$stdin_file` payload).
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
    assert_eq!(finished[1].1, "MaxReviewCyclesExceeded");
    assert_eq!(finished[2].1, "Converged");
}

/// Issue #72 acceptance criterion: `--fail-fast` stops the batch at the
/// first non-converged intent, recording every intent after it as
/// `Skipped` -- never even attempted (no `"... started"`/`"... finished:
/// ..."` line for it at all, and no `runs` row in SQLite).
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
    assert_eq!(finished[0].1, "MaxReviewCyclesExceeded");
}

/// Issue #72: `--intents-file` entries run first (file order), followed by
/// any `--intent` flags, in the order given -- and the two sources combine
/// rather than one overriding the other.
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

/// Issue #72: neither `--intent` nor `--intents-file` is required to be
/// present individually (each alone would make the other pointless to
/// support), but at least one intent must result from the two combined --
/// rejected as a clean CLI error rather than starting a run with an empty
/// intent.
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

/// Issue #72 review, LOW 2: an `--intents-file` that was actually supplied
/// but contributed zero intents (every line blank or a comment) must name
/// the file in the error -- the generic "no intent provided" message from
/// [`e2e_run_without_any_intent_is_a_clean_cli_error`] reads as if no file
/// was given at all, which is misleading when one very much was.
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

/// Issue #72 acceptance criterion: a Ctrl-C (`SIGINT`) during batch mode lets
/// the in-flight intent's own child run to completion rather than killing
/// it, then records every intent after it as `Skipped` (never even
/// attempted) instead of continuing the batch -- the summary is still
/// printed and the process still exits non-zero.
///
/// Driven against the real compiled binary via a raw [`SyncCommand`] (not
/// `warden_command()`/`assert_cmd`, same reason as
/// `e2e_run_survives_a_closed_stdout_without_panicking` above: this needs to
/// observe -- and signal -- the batch parent while it is still running,
/// which `assert_cmd::Command` does not expose). The first intent's coder
/// fragment sleeps briefly (deterministically identified by a marker in its
/// own intent text, checked against `$stdin_file`, exactly like
/// `e2e_batch_continues_past_a_non_converged_intent_by_default`'s
/// `BATCH_FORCE_FAIL_MARKER` above) so there is a real window, after its
/// child has printed its own `"... started"` line but strictly before it
/// converges, in which to send `SIGINT` to the *batch parent's* pid only
/// (never the grandchild) -- proving the batch-level cancellation flag,
/// not a killed child, is what stops the remaining intents.
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

    // Reads stdout live (rather than waiting for the whole process to
    // finish) so `SIGINT` can be sent while the first intent's own child is
    // still running its 2s sleep -- well before it converges.
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
