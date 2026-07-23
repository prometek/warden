//! Bounded reads of a cycle's HEAD commit and diff (ADR-0012), capped at
//! [`MAX_DIFF_BYTES`] so a single outsized commit can't wedge a run.

use super::*;

pub(super) async fn read_head_commit(worktree_path: &Path) -> Result<String> {
    // `NO_HOST_HOOKS` (issue #49 review, HIGH, defense-in-depth) -- see
    // `crate::git_util`'s own docs.
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(NO_HOST_HOOKS)
        .args(["rev-parse", "HEAD"])
        .output()
        .await?;

    if !output.status.success() {
        return Err(WardenError::Worktree(WorktreeError::GitCommandFailed {
            command: format!("git -C {} rev-parse HEAD", worktree_path.display()),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Hard cap on how many bytes of a cycle's diff [`read_diff`] will ever hand
/// to a reviewer/tester over stdin (M1, issue #20 review): the coder runs
/// against a real repository the user chose, so nothing bounds how large a
/// single cycle's diff can be -- reading it unbounded into memory, then
/// JSON-escaping it (another full copy, `agent_wire::to_json`), then piping
/// it, risks a single outsized commit wedging a run. 8 MiB comfortably
/// covers any diff a reviewer/tester could plausibly act on; a legitimate
/// review/test cycle operates on a handful of files at a time, never a
/// repository-sized rewrite.
const MAX_DIFF_BYTES: usize = 8 * 1024 * 1024;

/// Applies [`MAX_DIFF_BYTES`] to a raw diff capture, appending
/// [`DIFF_TRUNCATED_MARKER`] (`warden_core::agent_wire`, part of the wire
/// contract so an agent-side consumer can discover it) only when truncation
/// actually happened. Pulled out of [`read_diff`] so the truncation
/// behaviour itself is unit-testable without spawning `git` against a
/// multi-megabyte fixture.
fn cap_diff(raw: &[u8], max_bytes: usize) -> String {
    if raw.len() <= max_bytes {
        return String::from_utf8_lossy(raw).into_owned();
    }
    // `from_utf8_lossy` already handles a byte-offset cut that lands mid
    // multi-byte character (replaces it with U+FFFD), the same convention
    // used everywhere else agent-adjacent bytes are decoded in this file.
    let mut diff = String::from_utf8_lossy(&raw[..max_bytes]).into_owned();
    diff.push_str(DIFF_TRUNCATED_MARKER);
    diff
}

/// Reads the `git diff base..target` text from `worktree_path` (ADR-0012,
/// issue #20 Scope B) -- this is the reviewer/tester's `AgentInputMessage::diff`.
/// Run against the worktree that's already checked out at `target` rather
/// than the main repo: both commits are equally reachable from either
/// (worktrees share the main repo's object store), but this must run before
/// the worktree is removed, while `target` is still guaranteed reachable
/// there. An empty result (identical `base`/`target`, e.g. a coder that
/// committed no changes) is a normal outcome, not an error.
///
/// Capped at [`MAX_DIFF_BYTES`] (M1, issue #20 review) via a bounded read off
/// `git diff`'s stdout pipe: everything past the cap is still drained to
/// `tokio::io::sink()` (never buffered) so `git diff` never blocks writing to
/// a pipe nobody is fully reading, without reintroducing unbounded memory
/// use.
///
/// `-c color.ui=false`, `--no-color`, `--no-ext-diff` and `--no-textconv`
/// neutralize the repo's (or the invoking user's global) git config, which
/// would otherwise be free to inject ANSI escapes, run an external diff
/// driver, or substitute a `.gitattributes`-configured `textconv` filter's
/// output for the real file content in a payload an agent has to parse as
/// plain JSON. Verified against real git behaviour (issue #20 review, BUG
/// 2): `core.textconv` is not a real git config key (silently ignored, so
/// `--no-textconv` is the flag that actually matters), and `-c
/// diff.external=` does not neutralize a configured `diff.external` the way
/// it looks like it should (`--no-ext-diff` is what actually disables it
/// cleanly). `--` separates `range` from a (here absent, but
/// defense-in-depth) pathspec.
pub(super) async fn read_diff(worktree_path: &Path, base: &str, target: &str) -> Result<String> {
    use tokio::io::AsyncReadExt;

    let range = format!("{base}..{target}");
    // `NO_HOST_HOOKS` (issue #49 review, HIGH, defense-in-depth) -- see
    // `crate::git_util`'s own docs.
    let mut child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(NO_HOST_HOOKS)
        .args(["-c", "color.ui=false"])
        .args([
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            &range,
            "--",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // Both streams are requested `Stdio::piped()` two lines above, so `None`
    // would mean `tokio::process::Command` broke its own contract. Surface it
    // as an error anyway rather than panicking (code-standards.md: "Aucun
    // `unwrap()` ni `expect()` hors tests").
    let mut stdout_handle = child.stdout.take().ok_or_else(|| {
        std::io::Error::other("git diff child has no stdout despite being spawned with a pipe")
    })?;
    let mut stderr_handle = child.stderr.take().ok_or_else(|| {
        std::io::Error::other("git diff child has no stderr despite being spawned with a pipe")
    })?;

    // Bounded read (M1): caps how much of `git diff`'s stdout is ever
    // buffered in memory, then drains anything left past the cap straight to
    // `tokio::io::sink()` -- discarded as it's read, never buffered -- so
    // `git` never blocks writing to a full stdout pipe nobody is still
    // reading (the same pipe-deadlock hazard `process::wait` documents for
    // stdin/stdout). A read error on either half is propagated rather than
    // swallowed: a partial `buffer` from a mid-read I/O failure must not be
    // handed back to the caller indistinguishable from a genuinely complete
    // (or cap-truncated) diff.
    let stdout_task = async move {
        let mut limited = (&mut stdout_handle).take(MAX_DIFF_BYTES as u64 + 1);
        let mut buffer = Vec::new();
        limited.read_to_end(&mut buffer).await?;
        tokio::io::copy(&mut stdout_handle, &mut tokio::io::sink()).await?;
        Ok::<Vec<u8>, std::io::Error>(buffer)
    };
    let stderr_task = async move {
        let mut buffer = Vec::new();
        stderr_handle.read_to_end(&mut buffer).await?;
        Ok::<Vec<u8>, std::io::Error>(buffer)
    };

    let (stdout_result, stderr_result, status_result) =
        tokio::join!(stdout_task, stderr_task, child.wait());
    let status = status_result?;
    let stdout_buf = stdout_result?;
    let stderr_buf = stderr_result?;

    if !status.success() {
        return Err(WardenError::Worktree(WorktreeError::GitCommandFailed {
            command: format!("git -C {} diff {range}", worktree_path.display()),
            exit_code: status.code(),
            stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        }));
    }

    Ok(cap_diff(&stdout_buf, MAX_DIFF_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::test_support::*;
    use std::process::Command as SyncCommand;

    #[test]
    fn cap_diff_returns_the_input_unchanged_when_under_the_cap() {
        let raw = b"diff --git a/x b/x\n+hello\n";
        assert_eq!(cap_diff(raw, 1024), String::from_utf8_lossy(raw));
    }

    #[test]
    fn cap_diff_truncates_and_appends_a_marker_when_over_the_cap() {
        let raw = vec![b'x'; 10];
        let capped = cap_diff(&raw, 4);
        assert!(
            capped.starts_with("xxxx"),
            "expected the first 4 bytes to survive truncation: {capped:?}"
        );
        assert!(
            capped.contains(DIFF_TRUNCATED_MARKER),
            "expected the truncation marker to be appended: {capped:?}"
        );
        // Exactly-at-the-cap input must not be treated as truncated.
        let exact = vec![b'x'; 4];
        assert!(!cap_diff(&exact, 4).contains(DIFF_TRUNCATED_MARKER));
    }

    #[tokio::test]
    async fn read_diff_returns_the_textual_change_between_two_commits() {
        let dir = init_test_repo();
        let base = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let base_sha = String::from_utf8_lossy(&base.stdout).trim().to_string();

        std::fs::write(dir.path().join("notes.txt"), "distinctive-marker-line\n").unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["add", "notes.txt"])
            .status()
            .unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args([
                "-c",
                "user.email=test@warden.local",
                "-c",
                "user.name=warden-test",
                "commit",
                "-q",
                "-m",
                "add notes",
            ])
            .status()
            .unwrap();
        let target = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let target_sha = String::from_utf8_lossy(&target.stdout).trim().to_string();

        let diff = read_diff(dir.path(), &base_sha, &target_sha).await.unwrap();
        assert!(
            diff.contains("distinctive-marker-line"),
            "expected the diff to contain the change: {diff:?}"
        );
    }

    #[tokio::test]
    async fn read_diff_returns_an_empty_string_for_identical_commits() {
        let dir = init_test_repo();
        let head = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let head_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();

        let diff = read_diff(dir.path(), &head_sha, &head_sha).await.unwrap();
        assert_eq!(
            diff, "",
            "a no-op diff must be an empty string, not an error"
        );
    }

    /// LOW (issue #20 review): the repo's own `color.ui=always` (which would
    /// normally make `git diff` emit ANSI escape codes) must be neutralized
    /// by `read_diff`, since the result rides inside a JSON payload an agent
    /// parses as plain text.
    #[tokio::test]
    async fn read_diff_ignores_the_repos_color_ui_always_config() {
        let dir = init_test_repo();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["config", "color.ui", "always"])
            .status()
            .unwrap();
        let base = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let base_sha = String::from_utf8_lossy(&base.stdout).trim().to_string();

        std::fs::write(dir.path().join("notes.txt"), "some content\n").unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["add", "notes.txt"])
            .status()
            .unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args([
                "-c",
                "user.email=test@warden.local",
                "-c",
                "user.name=warden-test",
                "commit",
                "-q",
                "-m",
                "add notes",
            ])
            .status()
            .unwrap();
        let target = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let target_sha = String::from_utf8_lossy(&target.stdout).trim().to_string();

        let diff = read_diff(dir.path(), &base_sha, &target_sha).await.unwrap();
        assert!(
            !diff.contains('\u{1b}'),
            "diff must contain no ANSI escape codes despite color.ui=always: {diff:?}"
        );
    }

    /// `cap_diff`'s exact boundary: cap-1 and cap-exact bytes must survive
    /// untouched and unmarked; cap+1 must truncate at exactly `max_bytes`
    /// and be marked. Complements the coder's own `cap_diff` tests (which
    /// used a 4-byte cap with 10/4-byte inputs) with the literal ±1
    /// boundary the task calls out.
    #[test]
    fn cap_diff_boundary_is_exact_at_cap_minus_one_cap_and_cap_plus_one() {
        let cap = 16;

        let under = vec![b'a'; cap - 1];
        let result = cap_diff(&under, cap);
        assert_eq!(result, String::from_utf8_lossy(&under));
        assert!(!result.contains(DIFF_TRUNCATED_MARKER));

        let exact = vec![b'a'; cap];
        let result = cap_diff(&exact, cap);
        assert_eq!(result, String::from_utf8_lossy(&exact));
        assert!(
            !result.contains(DIFF_TRUNCATED_MARKER),
            "input exactly at the cap must not be treated as truncated"
        );

        let over = vec![b'a'; cap + 1];
        let result = cap_diff(&over, cap);
        assert!(result.starts_with(&"a".repeat(cap)));
        assert!(result.contains(DIFF_TRUNCATED_MARKER));
        assert_eq!(
            result.len(),
            cap + DIFF_TRUNCATED_MARKER.len(),
            "exactly one byte over the cap must still truncate to exactly `cap` content bytes"
        );
    }

    /// M1 intent: a diff under the cap must reach the agent byte-exact, not
    /// merely "close enough" -- compares `read_diff`'s output directly
    /// against a plain `git diff` invocation over the same range, not just
    /// a substring check.
    #[tokio::test]
    async fn read_diff_under_the_cap_is_byte_exact_against_plain_git_diff() {
        let dir = init_test_repo();
        let base = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let base_sha = String::from_utf8_lossy(&base.stdout).trim().to_string();

        std::fs::write(dir.path().join("small.txt"), "line one\nline two\n").unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["add", "small.txt"])
            .status()
            .unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args([
                "-c",
                "user.email=test@warden.local",
                "-c",
                "user.name=warden-test",
                "commit",
                "-q",
                "-m",
                "add small file",
            ])
            .status()
            .unwrap();
        let target = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let target_sha = String::from_utf8_lossy(&target.stdout).trim().to_string();

        let expected = SyncCommand::new("git")
            .current_dir(dir.path())
            .args([
                "diff",
                "--no-color",
                "--no-ext-diff",
                &format!("{base_sha}..{target_sha}"),
            ])
            .output()
            .unwrap();
        let expected_text = String::from_utf8_lossy(&expected.stdout).into_owned();

        let diff = read_diff(dir.path(), &base_sha, &target_sha).await.unwrap();
        assert_eq!(
            diff, expected_text,
            "a diff under the cap must be byte-exact, not just 'contain' the change"
        );
        assert!(!diff.contains(DIFF_TRUNCATED_MARKER));
    }

    /// M1 intent, end-to-end through the real `git diff` subprocess (not
    /// just `cap_diff` in isolation): a diff over `MAX_DIFF_BYTES` must
    /// actually be truncated at the cap and carry the marker so the
    /// reviewer/tester can tell a truncated diff from a genuinely small
    /// one. Generates a real >8 MiB diff via git rather than asserting
    /// against a synthetic byte slice.
    #[tokio::test]
    async fn read_diff_over_the_cap_is_truncated_and_marked_via_real_git_diff() {
        let dir = init_test_repo();
        let base = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let base_sha = String::from_utf8_lossy(&base.stdout).trim().to_string();

        // A single ~9 MiB added file guarantees the diff itself exceeds
        // MAX_DIFF_BYTES (8 MiB) once the unified-diff framing (`+` prefix
        // per line, headers) is added on top of the file's own content.
        let line = "x".repeat(120);
        let mut content = String::with_capacity(9 * 1024 * 1024);
        while content.len() < 9 * 1024 * 1024 {
            content.push_str(&line);
            content.push('\n');
        }
        std::fs::write(dir.path().join("huge.txt"), &content).unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["add", "huge.txt"])
            .status()
            .unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args([
                "-c",
                "user.email=test@warden.local",
                "-c",
                "user.name=warden-test",
                "commit",
                "-q",
                "-m",
                "add huge file",
            ])
            .status()
            .unwrap();
        let target = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let target_sha = String::from_utf8_lossy(&target.stdout).trim().to_string();

        let diff = read_diff(dir.path(), &base_sha, &target_sha).await.unwrap();
        assert!(
            diff.contains(DIFF_TRUNCATED_MARKER),
            "a real diff exceeding MAX_DIFF_BYTES must be marked truncated"
        );
        assert_eq!(
            diff.len(),
            MAX_DIFF_BYTES + DIFF_TRUNCATED_MARKER.len(),
            "truncated diff length must be exactly the cap plus the marker, never more"
        );
    }

    /// M1 intent: the cap must bound *memory*, not just the returned
    /// string's length -- a `.take()`-based streaming read discards excess
    /// bytes without ever holding them all in memory at once, so this
    /// process's peak RSS growth while reading a diff should stay roughly
    /// constant regardless of how far over the cap the real diff is. A test
    /// that only checked `read_diff`'s output length would pass even if the
    /// implementation buffered the *entire* diff (or the entire excess)
    /// before truncating -- this samples this process's own RSS (via `ps`,
    /// no extra crate dependency) concurrently with the `read_diff` call to
    /// catch exactly that.
    ///
    /// Compares two diffs, one with a small excess over the cap and one
    /// with a much larger excess: a bounded implementation's RSS growth is
    /// close for both; an implementation that still buffers the excess (in
    /// full or in large chunks) shows growth that scales with the larger
    /// diff's size.
    fn self_rss_kb() -> i64 {
        let pid = std::process::id().to_string();
        let output = SyncCommand::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            .expect("spawn ps");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("ps -o rss= output must be an integer number of KiB")
    }

    /// Isolated worker for
    /// `read_diff_peak_memory_growth_is_bounded_regardless_of_how_far_over_the_cap_the_diff_is`
    /// below: measures *this process's* own peak RSS growth while
    /// `read_diff` reads a single diff whose size (in MiB) comes from
    /// `WARDEN_TEST_DIFF_TOTAL_MIB`, printing `RSS_GROWTH_KB=<n>` to
    /// stdout. `#[ignore]`d so ordinary `cargo test` runs never execute it
    /// directly -- it only runs when the parent test re-invokes this exact
    /// test binary (`std::env::current_exe`) as a fresh, single-test
    /// subprocess with `--test-threads=1`. That isolation is the point: RSS
    /// sampled from *this* shared test binary while dozens of unrelated
    /// tests run concurrently under `cargo test`'s default parallelism is
    /// too noisy to attribute to one test's own allocations (confirmed
    /// empirically -- an in-process version of this test flaked under
    /// `cargo test --workspace`, alternating pass/fail across runs with the
    /// same diff sizes and thresholds).
    #[tokio::test]
    #[ignore]
    async fn peak_rss_diff_worker_isolated_process() {
        let total_mib: usize = std::env::var("WARDEN_TEST_DIFF_TOTAL_MIB")
            .expect("WARDEN_TEST_DIFF_TOTAL_MIB must be set by the parent test")
            .parse()
            .expect("WARDEN_TEST_DIFF_TOTAL_MIB must be an integer");

        let dir = init_test_repo();
        let base = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let base_sha = String::from_utf8_lossy(&base.stdout).trim().to_string();

        let line = "y".repeat(120);
        let mut content = String::with_capacity(total_mib * 1024 * 1024);
        while content.len() < total_mib * 1024 * 1024 {
            content.push_str(&line);
            content.push('\n');
        }
        std::fs::write(dir.path().join("huge.txt"), &content).unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["add", "huge.txt"])
            .status()
            .unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args([
                "-c",
                "user.email=test@warden.local",
                "-c",
                "user.name=warden-test",
                "commit",
                "-q",
                "-m",
                "add huge file",
            ])
            .status()
            .unwrap();
        let target = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let target_sha = String::from_utf8_lossy(&target.stdout).trim().to_string();

        let baseline = self_rss_kb();
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(baseline));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let peak_clone = peak.clone();
        let stop_clone = stop.clone();
        let sampler = std::thread::spawn(move || {
            while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                let rss = self_rss_kb();
                peak_clone.fetch_max(rss, std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });

        let diff = read_diff(dir.path(), &base_sha, &target_sha).await.unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        sampler.join().unwrap();
        assert!(diff.contains(DIFF_TRUNCATED_MARKER), "sanity: over the cap");

        let growth_kb = peak.load(std::sync::atomic::Ordering::Relaxed) - baseline;
        println!("RSS_GROWTH_KB={growth_kb}");
    }

    /// M1 intent: the cap must bound *memory*, not just the returned
    /// string's length -- a `.take()`-based streaming read discards excess
    /// bytes without ever holding them all in memory at once, so peak RSS
    /// growth while reading a diff should stay roughly constant regardless
    /// of how far over the cap the real diff is. A test that only checked
    /// `read_diff`'s output length would pass even if the implementation
    /// buffered the *entire* diff (or the entire excess) before truncating
    /// -- this measures actual peak RSS via [`peak_rss_diff_worker_isolated_process`]
    /// (re-invoked as an isolated subprocess so unrelated tests running
    /// concurrently under `cargo test` can't pollute the measurement) to
    /// catch exactly that.
    ///
    /// Compares two diffs, one with a small excess over the cap and one
    /// with a much larger excess: a bounded implementation's RSS growth is
    /// close for both; an implementation that still buffers the excess (in
    /// full or in large chunks) shows growth that scales with the larger
    /// diff's size.
    #[test]
    fn read_diff_peak_memory_growth_is_bounded_regardless_of_how_far_over_the_cap_the_diff_is() {
        fn measure_rss_growth_kb(total_mib: usize) -> i64 {
            let exe = std::env::current_exe().expect("current_exe available for this test binary");
            let output = SyncCommand::new(&exe)
                .args([
                    "--exact",
                    "orchestrator::diff::tests::peak_rss_diff_worker_isolated_process",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("WARDEN_TEST_DIFF_TOTAL_MIB", total_mib.to_string())
                .output()
                .expect("spawn isolated subprocess for the RSS worker test");
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Under `--nocapture` libtest prints "test <name> ... " on the
            // same line immediately before the test's own stdout, so the
            // marker isn't necessarily at the start of a line -- search for
            // it as a substring instead.
            let after_marker = stdout.split("RSS_GROWTH_KB=").nth(1).unwrap_or_else(|| {
                panic!(
                    "isolated RSS worker subprocess did not print RSS_GROWTH_KB=... \
                             (exit status {:?}); stdout: {stdout:?}, stderr: {:?}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            after_marker
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .filter(|digits| !digits.is_empty())
                .expect("RSS_GROWTH_KB=... must be followed by an integer number of KiB")
                .parse()
                .expect("RSS_GROWTH_KB=... must be an integer number of KiB")
        }

        // ~9 MiB diff (~1 MiB excess over the 8 MiB cap) vs. ~90 MiB diff
        // (~82 MiB excess). If the excess is fully buffered rather than
        // streamed/discarded, the larger diff's peak RSS growth would be
        // many tens of MiB higher than the smaller diff's.
        let small_excess_growth_kb = measure_rss_growth_kb(9);
        let large_excess_growth_kb = measure_rss_growth_kb(90);

        let delta_kb = large_excess_growth_kb - small_excess_growth_kb;
        assert!(
            delta_kb < 20 * 1024,
            "peak RSS growth must stay roughly constant regardless of how far over the \
                 cap the diff is (the excess must be streamed/discarded, never buffered in \
                 full) -- small-excess growth {small_excess_growth_kb} KiB, \
                 large-excess growth {large_excess_growth_kb} KiB, delta {delta_kb} KiB"
        );
    }

    /// M1 intent: the repo's `diff.<driver>.textconv` (opted into via
    /// `.gitattributes`) must not be allowed to substitute the real file
    /// content in the diff payload -- a textconv filter runs arbitrary
    /// output in place of the actual change, which is exactly the kind of
    /// git-config-driven corruption `read_diff`'s doc comment claims to
    /// neutralize alongside `color.ui`/`diff.external`. Uses a textconv
    /// filter that emits the *same* fixed marker for every blob (so if it
    /// were applied, the "converted" before/after would be textually
    /// identical and the diff would come back empty) to prove textconv ran
    /// at all, distinct from just checking the marker text is absent.
    #[tokio::test]
    async fn read_diff_ignores_gitattributes_configured_textconv() {
        let dir = init_test_repo();

        std::fs::write(
            dir.path().join(".gitattributes"),
            "tracked.bin diff=faketextconv\n",
        )
        .unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["add", ".gitattributes"])
            .status()
            .unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args([
                "-c",
                "user.email=test@warden.local",
                "-c",
                "user.name=warden-test",
                "commit",
                "-q",
                "-m",
                "add gitattributes",
            ])
            .status()
            .unwrap();

        std::fs::write(dir.path().join("tracked.bin"), "real-content-v1\n").unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["add", "tracked.bin"])
            .status()
            .unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args([
                "-c",
                "user.email=test@warden.local",
                "-c",
                "user.name=warden-test",
                "commit",
                "-q",
                "-m",
                "add tracked.bin v1",
            ])
            .status()
            .unwrap();
        let base = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let base_sha = String::from_utf8_lossy(&base.stdout).trim().to_string();

        // A textconv filter that ignores its actual input and always
        // prints the same fixed line -- if applied, both sides of the diff
        // would "convert" to identical text and the diff would be empty.
        let script_path = dir.path().join("fake_textconv.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\necho textconv-marker-always-the-same\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args([
                "config",
                "diff.faketextconv.textconv",
                script_path.to_str().unwrap(),
            ])
            .status()
            .unwrap();

        std::fs::write(dir.path().join("tracked.bin"), "real-content-v2\n").unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["add", "tracked.bin"])
            .status()
            .unwrap();
        SyncCommand::new("git")
            .current_dir(dir.path())
            .args([
                "-c",
                "user.email=test@warden.local",
                "-c",
                "user.name=warden-test",
                "commit",
                "-q",
                "-m",
                "modify tracked.bin",
            ])
            .status()
            .unwrap();
        let target = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let target_sha = String::from_utf8_lossy(&target.stdout).trim().to_string();

        let diff = read_diff(dir.path(), &base_sha, &target_sha).await.unwrap();
        assert!(
            diff.contains("real-content-v1") && diff.contains("real-content-v2"),
            "the diff must show the real file content, not have been swallowed by a \
                 textconv filter that maps every blob to the same marker text: {diff:?}"
        );
        assert!(
            !diff.contains("textconv-marker-always-the-same"),
            "the textconv filter's output must never appear in the payload: {diff:?}"
        );
    }
}
