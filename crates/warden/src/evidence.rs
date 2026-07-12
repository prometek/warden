//! Evidence Capture Adapter (ADR-0009, issue #7): after a cycle's tester
//! reports a successful e2e run, captures a tangible, human-consultable
//! proof of the observed behaviour -- a Playwright screenshot/video for
//! web/UI projects, an asciinema terminal recording for CLI projects.
//!
//! Storage is two-phase (Architecture.md §7): artifacts land first on local
//! scratch storage under `<warden_home>/evidence/<run_id>/<cycle_number>/`,
//! outside any git working tree -- capture happens in the tester's own
//! ephemeral worktree, which the orchestrator removes right after this
//! returns, so nothing here can depend on that worktree surviving. Only
//! later, at convergence ([`commit_evidence_into_repo`]), does
//! `evidence.store_in_repo` (default `true`) copy those files into
//! `.warden/evidence/<cycle_number>/` inside a dedicated worktree and commit
//! them -- never pushed before `Finalize` (ADR-0007).

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;
use warden_core::{
    detect_project_type, select_evidence_tool, EvidenceTool, EvidenceType, ProjectMarkers,
};

use crate::db::EvidenceWithCycle;
use crate::error::{EvidenceError, Result, WardenError, WorktreeError};
use crate::process::{self, AgentCommand};
use crate::worktree::WorktreeManager;

/// One artifact an adapter produced, already copied onto local scratch
/// storage and tagged with the repo-relative path it will occupy once
/// `evidence.store_in_repo` commits it (see module docs).
#[derive(Debug, Clone)]
pub struct CapturedEvidence {
    pub evidence_type: EvidenceType,
    /// Absolute path to the artifact's current, pre-commit location on
    /// local scratch storage.
    pub scratch_path: PathBuf,
    /// `.warden/evidence/<cycle_number>/<filename>` -- where this artifact
    /// lands inside the repo once committed.
    pub repo_relative_path: String,
    pub description: String,
}

/// Everything an [`EvidenceAdapter`] needs to capture evidence for one
/// cycle.
pub struct EvidenceCaptureContext<'a> {
    /// The tester's own worktree, checked out at the cycle's commit --
    /// still open when capture runs (the caller removes it right after).
    pub worktree_path: &'a Path,
    /// Local scratch directory this cycle's artifacts are written to
    /// (`<warden_home>/evidence/<run_id>/<cycle_number>/`); created by the
    /// caller before capture starts.
    pub scratch_dir: &'a Path,
    pub cycle_number: u32,
    /// The command whose execution asciinema records verbatim (the
    /// project's own tester command, since that's the most faithful "what a
    /// human would see" proxy Warden has). Playwright ignores this -- it
    /// drives its own test runner instead.
    pub record_command: &'a AgentCommand,
    pub cancel: CancellationToken,
}

/// A capture tool: runs whatever it needs to inside
/// [`EvidenceCaptureContext::worktree_path`] and returns every artifact it
/// produced, already staged on scratch storage.
#[allow(async_fn_in_trait)]
pub trait EvidenceAdapter {
    async fn capture(&self, ctx: &EvidenceCaptureContext<'_>) -> Result<Vec<CapturedEvidence>>;
}

/// Repo-relative destination for one captured file, stable regardless of
/// when (or whether) it's actually committed.
fn repo_relative_path(cycle_number: u32, file_name: &str) -> String {
    format!(".warden/evidence/{cycle_number}/{file_name}")
}

// ---------------------------------------------------------------------------
// Playwright adapter (web/UI projects)
// ---------------------------------------------------------------------------

const PLAYWRIGHT_OUTPUT_DIR: &str = "test-results";
const PLAYWRIGHT_IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg"];
const PLAYWRIGHT_VIDEO_EXTENSIONS: &[&str] = &["webm", "mp4"];

/// Runs Playwright's own test runner in headless mode and collects the
/// screenshots/videos it wrote out. Assumes the target repo already has its
/// own `playwright.config.*` with `screenshot`/`video` capture configured --
/// Warden only invokes the runner and harvests its default output directory
/// (`test-results/`); it does not configure Playwright itself.
pub struct PlaywrightAdapter;

impl EvidenceAdapter for PlaywrightAdapter {
    async fn capture(&self, ctx: &EvidenceCaptureContext<'_>) -> Result<Vec<CapturedEvidence>> {
        let command = AgentCommand::new("npx", ["--yes", "playwright", "test", "--reporter=list"]);
        let outcome =
            process::spawn_and_wait(&command, ctx.worktree_path, ctx.cancel.clone()).await?;

        if outcome.exit_code != 0 {
            return Err(EvidenceError::CommandFailed {
                tool: "playwright",
                exit_code: Some(outcome.exit_code),
                stderr: outcome.stderr,
            }
            .into());
        }

        let output_dir = ctx.worktree_path.join(PLAYWRIGHT_OUTPUT_DIR);
        let artifact_paths = collect_files_recursively(&output_dir).await?;

        let mut captured = Vec::new();
        for path in artifact_paths {
            let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            let extension = extension.to_ascii_lowercase();
            let evidence_type = if PLAYWRIGHT_IMAGE_EXTENSIONS.contains(&extension.as_str()) {
                EvidenceType::Image
            } else if PLAYWRIGHT_VIDEO_EXTENSIONS.contains(&extension.as_str()) {
                EvidenceType::Video
            } else {
                continue;
            };

            captured.push(
                stage_on_scratch(
                    &path,
                    &output_dir,
                    ctx.scratch_dir,
                    ctx.cycle_number,
                    evidence_type,
                    "Playwright capture from the cycle's e2e test run",
                )
                .await?,
            );
        }

        if captured.is_empty() {
            return Err(EvidenceError::NoArtifactsProduced {
                tool: "playwright",
                path: output_dir,
            }
            .into());
        }

        Ok(captured)
    }
}

// ---------------------------------------------------------------------------
// asciinema adapter (CLI projects)
// ---------------------------------------------------------------------------

const ASCIINEMA_CAST_FILE_NAME: &str = "session.cast";

/// Records `ctx.record_command`'s terminal session via `asciinema rec`,
/// writing the `.cast` file directly onto scratch storage -- no separate
/// harvesting step needed, unlike Playwright.
pub struct AsciinemaAdapter;

impl EvidenceAdapter for AsciinemaAdapter {
    async fn capture(&self, ctx: &EvidenceCaptureContext<'_>) -> Result<Vec<CapturedEvidence>> {
        let scratch_path = ctx.scratch_dir.join(ASCIINEMA_CAST_FILE_NAME);
        let recorded_command = shell_join(ctx.record_command);

        let command = AgentCommand::new(
            "asciinema",
            [
                "rec".to_string(),
                "--quiet".to_string(),
                "--overwrite".to_string(),
                "--command".to_string(),
                recorded_command,
                scratch_path.display().to_string(),
            ],
        );
        let outcome =
            process::spawn_and_wait(&command, ctx.worktree_path, ctx.cancel.clone()).await?;

        if outcome.exit_code != 0 {
            return Err(EvidenceError::CommandFailed {
                tool: "asciinema",
                exit_code: Some(outcome.exit_code),
                stderr: outcome.stderr,
            }
            .into());
        }

        if !tokio::fs::try_exists(&scratch_path).await? {
            return Err(EvidenceError::NoArtifactsProduced {
                tool: "asciinema",
                path: scratch_path,
            }
            .into());
        }

        Ok(vec![CapturedEvidence {
            evidence_type: EvidenceType::Other,
            repo_relative_path: repo_relative_path(ctx.cycle_number, ASCIINEMA_CAST_FILE_NAME),
            description: "asciinema recording of the cycle's tester command".to_string(),
            scratch_path,
        }])
    }
}

/// Renders `command` back into a single shell-invocable string -- the
/// inverse of `main.rs`'s `parse_agent_command` whitespace split. Good
/// enough for the same reason that split is: no quoting/escaping support is
/// needed yet (main.rs: "agents that need quoting/escaping should be
/// wrapped in their own script").
fn shell_join(command: &AgentCommand) -> String {
    std::iter::once(command.program.as_str())
        .chain(command.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Tool selection + top-level capture entry point
// ---------------------------------------------------------------------------

/// Selects an adapter from `override_tool` (`RunConfig::evidence_tool`, the
/// `evidence.tool` config, ADR-0009) or the detected project type, then runs
/// it.
pub async fn capture_evidence(
    project_markers: &ProjectMarkers,
    override_tool: Option<EvidenceTool>,
    ctx: &EvidenceCaptureContext<'_>,
) -> Result<Vec<CapturedEvidence>> {
    let project_type = detect_project_type(project_markers);
    let tool = select_evidence_tool(project_type, override_tool);

    match tool {
        EvidenceTool::Playwright => PlaywrightAdapter.capture(ctx).await,
        EvidenceTool::Asciinema => AsciinemaAdapter.capture(ctx).await,
    }
}

// ---------------------------------------------------------------------------
// Project-type scanning (I/O boundary for warden_core::detect_project_type)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
struct PackageJson {
    #[serde(default)]
    dependencies: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: std::collections::BTreeMap<String, String>,
}

/// Gathers the filesystem facts `warden_core::detect_project_type`
/// classifies: the repo root's entry names, plus `package.json`'s
/// dependency keys if present. A shallow, one-level scan is deliberate --
/// ADR-0009 calls for "détection simple", not a full dependency-tree walk.
pub async fn scan_project_markers(repo_path: &Path) -> Result<ProjectMarkers> {
    let mut root_entries = Vec::new();
    let mut entries = tokio::fs::read_dir(repo_path).await?;
    while let Some(entry) = entries.next_entry().await? {
        if let Some(name) = entry.file_name().to_str() {
            root_entries.push(name.to_string());
        }
    }

    let package_json_dependencies = read_package_json_dependencies(repo_path).await?;

    Ok(ProjectMarkers {
        root_entries,
        package_json_dependencies,
    })
}

/// The union of `dependencies`/`devDependencies` keys from the repo root's
/// `package.json`, or an empty list if that file doesn't exist. A malformed
/// `package.json` is a boundary error, not silently treated as "no
/// dependencies" (code-standards.md: "no silent fallback").
async fn read_package_json_dependencies(repo_path: &Path) -> Result<Vec<String>> {
    let path = repo_path.join("package.json");
    if !tokio::fs::try_exists(&path).await? {
        return Ok(Vec::new());
    }

    let contents = tokio::fs::read_to_string(&path).await?;
    let parsed: PackageJson =
        serde_json::from_str(&contents).map_err(|source| WardenError::InvalidPackageJson {
            path: path.clone(),
            source,
        })?;

    let mut dependencies: Vec<String> = parsed
        .dependencies
        .into_keys()
        .chain(parsed.dev_dependencies.into_keys())
        .collect();
    dependencies.sort();
    dependencies.dedup();
    Ok(dependencies)
}

/// Recursively lists every file (not directory) under `dir`, or an empty
/// list if `dir` doesn't exist at all (Playwright's `test-results/` is only
/// created when it actually captures something).
async fn collect_files_recursively(dir: &Path) -> Result<Vec<PathBuf>> {
    if !tokio::fs::try_exists(dir).await? {
        return Ok(Vec::new());
    }

    let mut stack = vec![dir.to_path_buf()];
    let mut files = Vec::new();
    while let Some(current) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&current).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// Copies `source` (found under `base_dir`) onto `scratch_dir`, flattening
/// its path relative to `base_dir` into the file name (so nested Playwright
/// output directories -- one per test -- can't collide once flattened), and
/// returns the resulting [`CapturedEvidence`].
async fn stage_on_scratch(
    source: &Path,
    base_dir: &Path,
    scratch_dir: &Path,
    cycle_number: u32,
    evidence_type: EvidenceType,
    description: &str,
) -> Result<CapturedEvidence> {
    let relative = source.strip_prefix(base_dir).unwrap_or(source);
    let flattened_name = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("__");

    let scratch_path = scratch_dir.join(&flattened_name);
    tokio::fs::copy(source, &scratch_path).await?;

    Ok(CapturedEvidence {
        evidence_type,
        scratch_path,
        repo_relative_path: repo_relative_path(cycle_number, &flattened_name),
        description: description.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Committing captured evidence into the repo (ADR-0007/ADR-0009: never
// pushed before Finalize, but committed locally beforehand so the converged
// commit already carries it).
// ---------------------------------------------------------------------------

/// Commits every evidence artifact captured across `run_id`'s cycles into
/// the repo, under `.warden/evidence/<cycle_number>/`, on top of
/// `base_commit` -- the last step before a run's converged commit is
/// recorded (ADR-0009: "poussés avec le contenu final au Finalize -- jamais
/// avant", ADR-0007). Returns `base_commit` unchanged if `evidence` is
/// empty, so callers can unconditionally treat the return value as "the
/// commit to converge on" either way.
///
/// Creates its own dedicated worktree (role `"evidence"`) rather than
/// reusing the coder's or tester's -- both are already removed by the time a
/// run reaches `RunState::Converged`.
pub async fn commit_evidence_into_repo(
    worktree_manager: &WorktreeManager,
    main_repo_path: &Path,
    warden_home: &Path,
    run_id: &str,
    base_commit: &str,
    evidence: &[EvidenceWithCycle],
) -> Result<String> {
    if evidence.is_empty() {
        return Ok(base_commit.to_string());
    }

    let worktree = worktree_manager
        .create(run_id, "evidence", base_commit)
        .await?;

    for entry in evidence {
        let file_name = Path::new(&entry.evidence.file_path)
            .file_name()
            .ok_or_else(|| EvidenceError::InvalidStoredEvidencePath {
                file_path: entry.evidence.file_path.clone(),
            })?;
        let scratch_path = warden_home
            .join("evidence")
            .join(run_id)
            .join(entry.cycle_number.to_string())
            .join(file_name);
        let destination = worktree.path().join(&entry.evidence.file_path);
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::copy(&scratch_path, &destination).await?;
    }

    run_git(worktree.path(), &["add", ".warden/evidence"]).await?;
    run_git(
        worktree.path(),
        &[
            "commit",
            "--quiet",
            "-m",
            &format!("chore(evidence): attach captured evidence for run {run_id}"),
        ],
    )
    .await?;

    let new_commit = git_head_commit(worktree.path()).await?;

    // Same rationale as `orchestrator::protect_cycle_commit`: worktrees
    // share the main repo's object store, so this commit becomes ordinary
    // unreachable garbage the instant its worktree is removed unless
    // something else points at it.
    run_git(
        main_repo_path,
        &[
            "update-ref",
            &format!("refs/warden/runs/{run_id}/evidence"),
            &new_commit,
        ],
    )
    .await?;

    if let Err(error) = worktree.remove().await {
        tracing::warn!(%error, "failed to clean up evidence worktree after committing evidence");
    }

    Ok(new_commit)
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .await?;

    if !output.status.success() {
        return Err(WorktreeError::GitCommandFailed {
            command: format!("git -C {} {}", cwd.display(), args.join(" ")),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
        .into());
    }
    Ok(())
}

async fn git_head_commit(cwd: &Path) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "HEAD"])
        .output()
        .await?;

    if !output.status.success() {
        return Err(WorktreeError::GitCommandFailed {
            command: format!("git -C {} rev-parse HEAD", cwd.display()),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as SyncCommand;

    use tempfile::TempDir;

    use crate::db::Evidence;
    use crate::error::{ProcessError, WardenError};

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

    fn head_commit(repo: &Path) -> String {
        let output = SyncCommand::new("git")
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    // -----------------------------------------------------------------
    // scan_project_markers: the I/O boundary that feeds
    // warden_core::detect_project_type.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn scan_project_markers_collects_root_entries_and_merged_package_json_dependencies() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"lodash":"1.0.0"},"devDependencies":{"react":"18.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("index.js"), "console.log('hi')").unwrap();

        let markers = scan_project_markers(dir.path()).await.unwrap();

        assert!(markers.root_entries.contains(&"package.json".to_string()));
        assert!(markers.root_entries.contains(&"index.js".to_string()));
        assert_eq!(
            markers.package_json_dependencies,
            vec!["lodash".to_string(), "react".to_string()],
            "dependencies and devDependencies must be merged, sorted, and deduped"
        );
    }

    #[tokio::test]
    async fn scan_project_markers_returns_empty_dependencies_when_no_package_json_exists() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();

        let markers = scan_project_markers(dir.path()).await.unwrap();

        assert!(markers.package_json_dependencies.is_empty());
        assert_eq!(
            warden_core::detect_project_type(&markers),
            warden_core::ProjectType::Cli
        );
    }

    #[tokio::test]
    async fn scan_project_markers_detects_a_web_marker_file_end_to_end_into_project_type() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("index.html"), "<html></html>").unwrap();

        let markers = scan_project_markers(dir.path()).await.unwrap();

        assert_eq!(
            warden_core::detect_project_type(&markers),
            warden_core::ProjectType::Web
        );
    }

    #[tokio::test]
    async fn scan_project_markers_errors_on_malformed_package_json_instead_of_silently_ignoring_it()
    {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("package.json"), "{ not valid json").unwrap();

        let result = scan_project_markers(dir.path()).await;

        assert!(
            matches!(result, Err(WardenError::InvalidPackageJson { .. })),
            "a malformed package.json must be a typed error, not treated as \"no dependencies\": {result:?}"
        );
    }

    // -----------------------------------------------------------------
    // capture_evidence: tool selection dispatches to the right adapter,
    // and a missing tool binary is a typed, non-panicking error
    // (acceptance criterion 7 relies on this staying an ordinary `Err`).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn capture_evidence_override_beats_web_detection_and_dispatches_to_asciinema() {
        // Markers look unambiguously like a web project (an `index.html`
        // marker file) -- without an override this would select
        // Playwright. The override must still win (acceptance criterion
        // 2), which we observe here by checking *which binary* capture
        // actually tried to spawn: `asciinema`, not `npx`/playwright.
        let markers = warden_core::ProjectMarkers {
            root_entries: vec!["index.html".to_string()],
            package_json_dependencies: vec![],
        };
        let worktree_dir = TempDir::new().unwrap();
        let scratch_dir = TempDir::new().unwrap();
        let ctx = EvidenceCaptureContext {
            worktree_path: worktree_dir.path(),
            scratch_dir: scratch_dir.path(),
            cycle_number: 1,
            record_command: &AgentCommand::new("sh", ["-c", "true"]),
            cancel: CancellationToken::new(),
        };

        let result = capture_evidence(&markers, Some(EvidenceTool::Asciinema), &ctx).await;

        match result {
            Err(WardenError::Process(ProcessError::Spawn { command, .. })) => {
                assert_eq!(
                    command, "asciinema",
                    "override must dispatch to the asciinema adapter even though the project looks like Web"
                );
            }
            other => panic!(
                "expected a Spawn error for the (assumed absent) `asciinema` binary, got: {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn capture_evidence_missing_tool_binary_is_a_typed_error_not_a_panic() {
        let markers = warden_core::ProjectMarkers::default(); // Cli -> asciinema
        let worktree_dir = TempDir::new().unwrap();
        let scratch_dir = TempDir::new().unwrap();
        let ctx = EvidenceCaptureContext {
            worktree_path: worktree_dir.path(),
            scratch_dir: scratch_dir.path(),
            cycle_number: 1,
            record_command: &AgentCommand::new("sh", ["-c", "true"]),
            cancel: CancellationToken::new(),
        };

        // acceptance criterion 7: a missing tool must surface as an
        // ordinary Result::Err the caller can catch and log -- never a
        // panic that would take the whole run down with it.
        let result = capture_evidence(&markers, None, &ctx).await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------
    // commit_evidence_into_repo (acceptance criterion 4: artifacts are
    // stored locally first and only committed at convergence, under
    // `.warden/evidence/<cycle>/`, never touching the user's main repo
    // working tree/branch before that).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn commit_evidence_into_repo_returns_base_commit_unchanged_when_there_is_no_evidence() {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();
        let base_commit = head_commit(repo.path());

        let result = commit_evidence_into_repo(
            &worktree_manager,
            repo.path(),
            warden_home.path(),
            "run-no-evidence",
            &base_commit,
            &[],
        )
        .await
        .unwrap();

        assert_eq!(
            result, base_commit,
            "with no captured evidence, the commit to converge on must be unchanged"
        );
    }

    #[tokio::test]
    async fn commit_evidence_into_repo_commits_artifacts_under_dot_warden_evidence_cycle_dir() {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();
        let base_commit = head_commit(repo.path());
        let run_id = "run-with-evidence";

        // Simulates what `capture_evidence_for_cycle` already staged on
        // local scratch storage during the cycle, before this ever runs.
        let scratch_dir = warden_home.path().join("evidence").join(run_id).join("1");
        tokio::fs::create_dir_all(&scratch_dir).await.unwrap();
        tokio::fs::write(scratch_dir.join("screenshot.png"), b"fake-png-bytes")
            .await
            .unwrap();

        let evidence = vec![EvidenceWithCycle {
            cycle_number: 1,
            evidence: Evidence {
                id: "evidence-1".to_string(),
                cycle_id: "cycle-1".to_string(),
                finding_id: None,
                evidence_type: EvidenceType::Image,
                file_path: ".warden/evidence/1/screenshot.png".to_string(),
                description: "Playwright capture".to_string(),
                captured_at: "2026-01-01T00:00:00Z".to_string(),
            },
        }];

        let new_commit = commit_evidence_into_repo(
            &worktree_manager,
            repo.path(),
            warden_home.path(),
            run_id,
            &base_commit,
            &evidence,
        )
        .await
        .unwrap();

        assert_ne!(
            new_commit, base_commit,
            "committing evidence must produce a new commit on top of base_commit"
        );

        // The artifact must actually be present in that commit's tree...
        let show = SyncCommand::new("git")
            .current_dir(repo.path())
            .args([
                "show",
                &format!("{new_commit}:.warden/evidence/1/screenshot.png"),
            ])
            .output()
            .unwrap();
        assert!(
            show.status.success(),
            "expected .warden/evidence/1/screenshot.png inside the evidence commit"
        );
        assert_eq!(show.stdout, b"fake-png-bytes");

        // ...reachable via the protective ref (mirrors
        // orchestrator::protect_cycle_commit's rationale: worktrees share
        // the main repo's object store).
        let ref_lookup = SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", &format!("refs/warden/runs/{run_id}/evidence")])
            .output()
            .unwrap();
        assert!(ref_lookup.status.success());
        assert_eq!(
            String::from_utf8_lossy(&ref_lookup.stdout).trim(),
            new_commit
        );

        // ...and the main repo's own checked-out branch/working tree must
        // be completely untouched -- evidence is committed on an isolated
        // ref, never merged/checked out into the user's repo (ADR-0007:
        // "jamais avant Finalize").
        assert_eq!(
            head_commit(repo.path()),
            base_commit,
            "the main repo's checked-out HEAD must not move"
        );
        let status = SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            status.stdout.is_empty(),
            "the main repo's working tree must stay clean: {:?}",
            String::from_utf8_lossy(&status.stdout)
        );
        assert!(
            !repo.path().join(".warden").exists(),
            "evidence must never land inside the main repo's own working tree"
        );
    }
}
