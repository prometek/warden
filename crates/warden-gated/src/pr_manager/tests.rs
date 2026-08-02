use super::*;
use warden_core::{FindingSource, Severity};

#[test]
fn detects_fixes_keyword_case_insensitively() {
    let linked = detect_linked_issue("FIXES #123: handle token expiry").unwrap();
    assert_eq!(linked.number, 123);
    assert_eq!(linked.keyword, "FIXES");
}

#[test]
fn detects_closes_and_resolves_keywords() {
    assert_eq!(detect_linked_issue("closes #7").unwrap().number, 7);
    assert_eq!(
        detect_linked_issue("Resolves #42 today").unwrap().number,
        42
    );
}

#[test]
fn finds_the_reference_anywhere_in_a_multi_line_intent() {
    let intent = "Add JWT expiry handling.\n\nFixes #99\n\nAlso cleans up logging.";
    assert_eq!(detect_linked_issue(intent).unwrap().number, 99);
}

#[test]
fn returns_none_when_the_intent_does_not_reference_an_issue() {
    assert!(detect_linked_issue("Add JWT expiry handling").is_none());
}

#[test]
fn does_not_match_a_bare_issue_number_without_a_keyword() {
    assert!(detect_linked_issue("see #123 for context").is_none());
}

#[test]
fn does_not_match_fixes_without_a_hash() {
    assert!(detect_linked_issue("fixes 123").is_none());
}

#[test]
fn generates_a_title_from_the_first_non_blank_line() {
    let title = generate_pr_title("\n  Add JWT expiry handling\nmore detail below").unwrap();
    assert_eq!(title, "Add JWT expiry handling");
}

#[test]
fn truncates_an_overly_long_first_line() {
    let long_line = "a".repeat(200);
    let title = generate_pr_title(&long_line).unwrap();
    assert_eq!(title.chars().count(), MAX_GENERATED_TITLE_LEN);
    assert!(title.ends_with('…'));
}

#[test]
fn rejects_a_blank_intent_rather_than_inventing_a_title() {
    assert!(matches!(
        generate_pr_title("   \n  \n"),
        Err(GatedError::EmptyIntent)
    ));
}

#[test]
fn body_leads_with_the_issue_reference_when_linked() {
    let linked = LinkedIssue {
        number: 123,
        keyword: "Fixes".to_string(),
    };
    let body = open_draft_pr_body("Handle token expiry", Some(&linked));
    assert!(body.starts_with("Fixes #123"));
    assert!(body.contains("Handle token expiry"));
}

#[test]
fn body_has_no_issue_reference_when_none_is_linked() {
    let body = open_draft_pr_body("Handle token expiry", None);
    assert!(!body.contains('#'));
}

#[test]
fn formats_trailers_matching_the_architecture_doc_example() {
    let trailers = CommitTrailers {
        cycle: 3,
        findings_resolved: vec!["r-042".to_string()],
        agent: "coder".to_string(),
    };
    assert_eq!(
        trailers.format(),
        "Warden-Cycle: 3\nWarden-Findings-Resolved: r-042\nWarden-Agent: coder"
    );
}

#[test]
fn omits_findings_resolved_trailer_when_nothing_was_resolved_yet() {
    let trailers = CommitTrailers {
        cycle: 1,
        findings_resolved: vec![],
        agent: "coder".to_string(),
    };
    assert_eq!(trailers.format(), "Warden-Cycle: 1\nWarden-Agent: coder");
}

#[test]
fn joins_multiple_resolved_finding_ids() {
    let trailers = CommitTrailers {
        cycle: 2,
        findings_resolved: vec!["r-001".to_string(), "t-002".to_string()],
        agent: "doc".to_string(),
    };
    assert_eq!(
        trailers.format(),
        "Warden-Cycle: 2\nWarden-Findings-Resolved: r-001, t-002\nWarden-Agent: doc"
    );
}

#[test]
fn append_trailers_matches_the_full_architecture_doc_example() {
    let message = "fix: gère le cas d'expiration du token JWT\n\n\
            Corrige le finding remonté par le reviewer sur l'absence de\n\
            vérification d'expiration côté middleware auth.";
    let trailers = CommitTrailers {
        cycle: 3,
        findings_resolved: vec!["r-042".to_string()],
        agent: "coder".to_string(),
    };

    let full = append_trailers(message, &trailers);

    assert_eq!(
        full,
        "fix: gère le cas d'expiration du token JWT\n\n\
            Corrige le finding remonté par le reviewer sur l'absence de\n\
            vérification d'expiration côté middleware auth.\n\n\
            Warden-Cycle: 3\nWarden-Findings-Resolved: r-042\nWarden-Agent: coder\n"
    );
}

#[test]
fn append_trailers_trims_trailing_whitespace_before_the_blank_separator() {
    let trailers = CommitTrailers {
        cycle: 1,
        findings_resolved: vec![],
        agent: "coder".to_string(),
    };
    let full = append_trailers("fix: something\n\n\n", &trailers);
    assert_eq!(
        full,
        "fix: something\n\nWarden-Cycle: 1\nWarden-Agent: coder\n"
    );
}

fn reviewer_finding() -> Finding {
    Finding {
        source: FindingSource::role("reviewer"),
        severity: Severity::Blocking,
        file: Some("src/auth.rs".to_string()),
        description: "missing expiry check".to_string(),
        action: Some("add expiry check".to_string()),
    }
}

fn tester_finding() -> Finding {
    Finding {
        source: FindingSource::role("tester"),
        severity: Severity::Info,
        file: None,
        description: "consider adding an e2e test".to_string(),
        action: None,
    }
}

#[test]
fn comment_lists_findings_grouped_by_source() {
    let summary = CycleSummary {
        cycle_number: 3,
        findings: vec![reviewer_finding(), tester_finding()],
    };
    let comment = format_cycle_comment(&summary);
    assert!(comment.contains("cycle 3"));
    assert!(comment.contains("**Reviewer**"));
    assert!(comment.contains("missing expiry check"));
    assert!(comment.contains("(src/auth.rs)"));
    assert!(comment.contains("**Tester**"));
    assert!(comment.contains("consider adding an e2e test"));
}

#[test]
fn a_warden_sourced_finding_is_rendered_rather_than_silently_dropped() {
    let summary = CycleSummary {
        cycle_number: 2,
        findings: vec![Finding {
            source: FindingSource::Warden,
            severity: Severity::Blocking,
            file: Some(".warden/agents/reviewer.md".to_string()),
            description: "the coder's diff touches an agent definition".to_string(),
            action: None,
        }],
    };

    let comment = format_cycle_comment(&summary);

    assert!(comment.contains("**Warden**"), "{comment}");
    assert!(
        comment.contains("the coder's diff touches an agent definition"),
        "{comment}"
    );
    assert!(comment.contains(".warden/agents/reviewer.md"), "{comment}");
    assert!(
        !comment.contains("No findings raised this cycle."),
        "a cycle that raised a finding must never claim it raised none: {comment}"
    );
}

#[test]
fn a_ci_sourced_finding_is_rendered_rather_than_silently_dropped() {
    let summary = CycleSummary {
        cycle_number: 4,
        findings: vec![Finding {
            source: FindingSource::Ci,
            severity: Severity::Blocking,
            file: None,
            description: "build failed on the pushed commit".to_string(),
            action: None,
        }],
    };

    let comment = format_cycle_comment(&summary);

    assert!(comment.contains("**Ci**"), "{comment}");
    assert!(
        comment.contains("build failed on the pushed commit"),
        "{comment}"
    );
}

#[test]
fn comment_says_so_explicitly_when_no_findings_were_raised() {
    let summary = CycleSummary {
        cycle_number: 1,
        findings: vec![],
    };
    let comment = format_cycle_comment(&summary);
    assert!(comment.contains("No findings raised this cycle."));
}

#[test]
fn comment_always_states_it_is_informational_only() {
    let summary = CycleSummary {
        cycle_number: 1,
        findings: vec![],
    };
    let comment = format_cycle_comment(&summary);
    assert!(comment.contains("Informational only"));
}

struct RecordingProvider {
    calls: std::sync::Mutex<Vec<String>>,
    next_pr_number: u64,
}

impl RecordingProvider {
    fn new(next_pr_number: u64) -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            next_pr_number,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl PrProvider for RecordingProvider {
    async fn open_draft(&self, params: &OpenDraftParams<'_>) -> Result<PrHandle> {
        self.calls.lock().unwrap().push(format!(
            "open_draft({}, {}) body={}",
            params.branch, params.title, params.body
        ));
        Ok(PrHandle {
            number: self.next_pr_number,
        })
    }

    async fn post_comment(&self, pr: &PrHandle, body: &str) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("post_comment({}, {body})", pr.number));
        Ok(())
    }

    async fn mark_ready(&self, pr: &PrHandle) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("mark_ready({})", pr.number));
        Ok(())
    }

    async fn update_body(&self, pr: &PrHandle, body: &str) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("update_body({}, {body})", pr.number));
        Ok(())
    }
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

#[tokio::test]
async fn remote_branch_head_matches_the_exact_ref_not_a_sibling_suffix_match() {
    let origin = tempfile::TempDir::new().unwrap();
    run_git(origin.path(), &["init", "--bare", "--quiet"]);

    let seed = tempfile::TempDir::new().unwrap();
    run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
    run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
    run_git(seed.path(), &["config", "user.name", "warden-test"]);
    run_git(
        seed.path(),
        &["commit", "--quiet", "--allow-empty", "-m", "main tip"],
    );
    let main_sha = {
        let output = std::process::Command::new("git")
            .current_dir(seed.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    run_git(
        seed.path(),
        &[
            "push",
            "--quiet",
            &origin.path().display().to_string(),
            "main",
        ],
    );

    run_git(seed.path(), &["checkout", "--quiet", "-b", "feat/main"]);
    run_git(
        seed.path(),
        &["commit", "--quiet", "--allow-empty", "-m", "feat/main tip"],
    );
    run_git(
        seed.path(),
        &[
            "push",
            "--quiet",
            &origin.path().display().to_string(),
            "feat/main",
        ],
    );

    let gate_repo = tempfile::TempDir::new().unwrap();
    run_git(
        gate_repo.path(),
        &[
            "clone",
            "--bare",
            "--quiet",
            &seed.path().display().to_string(),
            ".",
        ],
    );
    run_git(
        gate_repo.path(),
        &[
            "remote",
            "set-url",
            "origin",
            &origin.path().display().to_string(),
        ],
    );

    let head = remote_branch_head(gate_repo.path(), "origin", "main")
        .await
        .unwrap();
    assert_eq!(
        head,
        Some(main_sha),
        "must resolve refs/heads/main exactly, not a sibling refs/heads/feat/main"
    );
}

fn gate_repo_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    tempfile::TempDir,
    String,
) {
    let origin = tempfile::TempDir::new().unwrap();
    run_git(origin.path(), &["init", "--bare", "--quiet"]);

    let seed = tempfile::TempDir::new().unwrap();
    run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
    run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
    run_git(seed.path(), &["config", "user.name", "warden-test"]);
    std::fs::write(seed.path().join("f.txt"), "skeleton\n").unwrap();
    run_git(seed.path(), &["add", "."]);
    run_git(seed.path(), &["commit", "--quiet", "-m", "skeleton"]);
    let commit_sha = {
        let output = std::process::Command::new("git")
            .current_dir(seed.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    run_git(
        seed.path(),
        &[
            "push",
            "--quiet",
            &origin.path().display().to_string(),
            "main",
        ],
    );

    let gate_repo = tempfile::TempDir::new().unwrap();
    run_git(
        gate_repo.path(),
        &[
            "clone",
            "--bare",
            "--quiet",
            &seed.path().display().to_string(),
            ".",
        ],
    );
    run_git(
        gate_repo.path(),
        &[
            "remote",
            "set-url",
            "origin",
            &origin.path().display().to_string(),
        ],
    );

    (origin, seed, gate_repo, commit_sha)
}

fn gate_repo_with_business_commit_on_top_of_skeleton(
) -> (tempfile::TempDir, tempfile::TempDir, String, String) {
    let origin = tempfile::TempDir::new().unwrap();
    run_git(origin.path(), &["init", "--bare", "--quiet"]);

    let seed = tempfile::TempDir::new().unwrap();
    run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
    run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
    run_git(seed.path(), &["config", "user.name", "warden-test"]);
    std::fs::write(seed.path().join("SKELETON.md"), "skeleton\n").unwrap();
    run_git(seed.path(), &["add", "."]);
    run_git(seed.path(), &["commit", "--quiet", "-m", "skeleton"]);
    let skeleton_sha = {
        let output = std::process::Command::new("git")
            .current_dir(seed.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    run_git(
        seed.path(),
        &[
            "push",
            "--quiet",
            &origin.path().display().to_string(),
            "main",
        ],
    );

    std::fs::write(seed.path().join("src_business.rs"), "fn business() {}\n").unwrap();
    run_git(seed.path(), &["add", "."]);
    run_git(seed.path(), &["commit", "--quiet", "-m", "business code"]);
    let business_sha = {
        let output = std::process::Command::new("git")
            .current_dir(seed.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    let gate_repo = tempfile::TempDir::new().unwrap();
    run_git(
        gate_repo.path(),
        &[
            "clone",
            "--bare",
            "--quiet",
            &seed.path().display().to_string(),
            ".",
        ],
    );
    run_git(
        gate_repo.path(),
        &[
            "remote",
            "set-url",
            "origin",
            &origin.path().display().to_string(),
        ],
    );

    (origin, gate_repo, skeleton_sha, business_sha)
}

#[tokio::test]
async fn open_draft_pushes_the_skeleton_and_opens_a_draft_pr() {
    let (origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
    let provider = RecordingProvider::new(7);

    let request = OpenDraftRequest {
        bare_repo_path: gate_repo.path(),
        skeleton_commit_sha: &commit_sha,
        branch: "warden/run-1",
        base_branch: "main",
        intent: "Fixes #123: handle token expiry",
    };

    let pr = open_draft(&request, &provider).await.unwrap();
    assert_eq!(pr.number, 7);

    let calls = provider.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("open_draft(warden/run-1,"));
    assert!(calls[0].contains("Fixes #123"));

    let output = std::process::Command::new("git")
        .current_dir(origin.path())
        .args(["log", "-1", "--format=%H", "refs/heads/warden/run-1"])
        .output()
        .unwrap();
    let origin_head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(origin_head, commit_sha, "skeleton commit must reach origin");
}

#[tokio::test]
async fn open_draft_falls_back_to_an_intent_derived_title_when_no_issue_is_linked() {
    let (_origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
    let provider = RecordingProvider::new(9);

    let intent = "Add JWT expiry handling";
    let request = OpenDraftRequest {
        bare_repo_path: gate_repo.path(),
        skeleton_commit_sha: &commit_sha,
        branch: "warden/run-2",
        base_branch: "main",
        intent,
    };

    let pr = open_draft(&request, &provider).await.unwrap();
    assert_eq!(pr.number, 9);

    let calls = provider.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with(&format!("open_draft(warden/run-2, {intent})")));
    assert!(
        !calls[0].contains('#'),
        "no linked issue means neither the generated title nor the body may \
             reference one: {}",
        calls[0]
    );
}

#[tokio::test]
async fn open_draft_never_pushes_content_beyond_the_given_skeleton_commit() {
    let (origin, gate_repo, skeleton_sha, _business_sha) =
        gate_repo_with_business_commit_on_top_of_skeleton();
    let provider = RecordingProvider::new(11);

    let request = OpenDraftRequest {
        bare_repo_path: gate_repo.path(),
        skeleton_commit_sha: &skeleton_sha,
        branch: "warden/run-3",
        base_branch: "main",
        intent: "Add a feature",
    };

    open_draft(&request, &provider).await.unwrap();

    let log_output = std::process::Command::new("git")
        .current_dir(origin.path())
        .args(["log", "-1", "--format=%H", "refs/heads/warden/run-3"])
        .output()
        .unwrap();
    let origin_head = String::from_utf8_lossy(&log_output.stdout)
        .trim()
        .to_string();
    assert_eq!(
        origin_head, skeleton_sha,
        "origin must land on exactly the skeleton commit, never the branch tip \
             that happens to sit on top of it locally"
    );

    let ls_tree = std::process::Command::new("git")
        .current_dir(origin.path())
        .args(["ls-tree", "-r", "--name-only", "refs/heads/warden/run-3"])
        .output()
        .unwrap();
    let files = String::from_utf8_lossy(&ls_tree.stdout);
    assert!(
        files.contains("SKELETON.md"),
        "skeleton file missing: {files}"
    );
    assert!(
        !files.contains("src_business.rs"),
        "business code must never reach origin via open_draft: {files}"
    );
}

#[tokio::test]
async fn open_draft_rejects_a_skeleton_sha_that_carries_business_content() {
    let (origin, gate_repo, _skeleton_sha, business_sha) =
        gate_repo_with_business_commit_on_top_of_skeleton();
    let provider = RecordingProvider::new(13);

    let request = OpenDraftRequest {
        bare_repo_path: gate_repo.path(),
        skeleton_commit_sha: &business_sha,
        branch: "warden/run-rejected",
        base_branch: "main",
        intent: "Add a feature",
    };

    let result = open_draft(&request, &provider).await;

    match result {
        Err(GatedError::SkeletonNotContentFree {
            commit_sha, files, ..
        }) => {
            assert_eq!(commit_sha, business_sha);
            assert_eq!(files, vec!["src_business.rs".to_string()]);
        }
        other => panic!("expected SkeletonNotContentFree, got {other:?}"),
    }

    assert!(
        provider.calls().is_empty(),
        "a rejected skeleton must never reach the PR provider"
    );

    let ref_check = std::process::Command::new("git")
        .current_dir(origin.path())
        .args(["rev-parse", "--verify", "refs/heads/warden/run-rejected"])
        .output()
        .unwrap();
    assert!(
        !ref_check.status.success(),
        "origin must never receive a push for a rejected skeleton"
    );
}

#[tokio::test]
async fn open_draft_rejects_a_skeleton_whose_tip_matches_base_but_whose_history_leaks_a_secret_file(
) {
    let origin = tempfile::TempDir::new().unwrap();
    run_git(origin.path(), &["init", "--bare", "--quiet"]);

    let seed = tempfile::TempDir::new().unwrap();
    run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
    run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
    run_git(seed.path(), &["config", "user.name", "warden-test"]);
    std::fs::write(seed.path().join("f.txt"), "skeleton\n").unwrap();
    run_git(seed.path(), &["add", "."]);
    run_git(seed.path(), &["commit", "--quiet", "-m", "skeleton"]);
    run_git(
        seed.path(),
        &[
            "push",
            "--quiet",
            &origin.path().display().to_string(),
            "main",
        ],
    );

    std::fs::write(seed.path().join("secret.rs"), "const KEY: &str = \"x\";\n").unwrap();
    run_git(seed.path(), &["add", "."]);
    run_git(seed.path(), &["commit", "--quiet", "-m", "add secret.rs"]);

    run_git(seed.path(), &["rm", "--quiet", "secret.rs"]);
    run_git(
        seed.path(),
        &["commit", "--quiet", "-m", "remove secret.rs"],
    );
    let tip_sha = {
        let output = std::process::Command::new("git")
            .current_dir(seed.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    let gate_repo = tempfile::TempDir::new().unwrap();
    run_git(
        gate_repo.path(),
        &[
            "clone",
            "--bare",
            "--quiet",
            &seed.path().display().to_string(),
            ".",
        ],
    );
    run_git(
        gate_repo.path(),
        &[
            "remote",
            "set-url",
            "origin",
            &origin.path().display().to_string(),
        ],
    );

    let provider = RecordingProvider::new(29);
    let request = OpenDraftRequest {
        bare_repo_path: gate_repo.path(),
        skeleton_commit_sha: &tip_sha,
        branch: "warden/run-leaky-history",
        base_branch: "main",
        intent: "Add a feature",
    };

    let result = open_draft(&request, &provider).await;
    match result {
        Err(GatedError::SkeletonNotContentFree {
            commit_sha, files, ..
        }) => {
            assert_eq!(commit_sha, tip_sha);
            assert_eq!(files, vec!["secret.rs".to_string()]);
        }
        other => panic!("expected SkeletonNotContentFree, got {other:?}"),
    }

    assert!(
        provider.calls().is_empty(),
        "a rejected skeleton must never reach the PR provider"
    );

    let ref_check = std::process::Command::new("git")
        .current_dir(origin.path())
        .args([
            "rev-parse",
            "--verify",
            "refs/heads/warden/run-leaky-history",
        ])
        .output()
        .unwrap();
    assert!(
        !ref_check.status.success(),
        "origin must never receive a push when the range leaks a secret file, \
             even if the tip tree matches base"
    );
}

#[tokio::test]
async fn open_draft_rejects_a_skeleton_whose_merge_commit_introduces_a_file_absent_from_both_parents(
) {
    let origin = tempfile::TempDir::new().unwrap();
    run_git(origin.path(), &["init", "--bare", "--quiet"]);

    let seed = tempfile::TempDir::new().unwrap();
    run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
    run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
    run_git(seed.path(), &["config", "user.name", "warden-test"]);
    std::fs::write(seed.path().join("f.txt"), "base\n").unwrap();
    run_git(seed.path(), &["add", "."]);
    run_git(seed.path(), &["commit", "--quiet", "-m", "base"]);
    run_git(
        seed.path(),
        &[
            "push",
            "--quiet",
            &origin.path().display().to_string(),
            "main",
        ],
    );

    run_git(seed.path(), &["checkout", "--quiet", "-b", "branch-a"]);
    run_git(
        seed.path(),
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "branch-a: no content",
        ],
    );

    run_git(seed.path(), &["checkout", "--quiet", "main"]);
    run_git(seed.path(), &["checkout", "--quiet", "-b", "branch-b"]);
    run_git(
        seed.path(),
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "branch-b: no content",
        ],
    );

    run_git(seed.path(), &["checkout", "--quiet", "branch-a"]);
    run_git(
        seed.path(),
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "branch-b",
            "-m",
            "merge branch-b into branch-a",
        ],
    );
    std::fs::write(seed.path().join("evil.txt"), "not a skeleton\n").unwrap();
    run_git(seed.path(), &["add", "evil.txt"]);
    run_git(seed.path(), &["commit", "--quiet", "--amend", "--no-edit"]);
    let merge_sha = {
        let output = std::process::Command::new("git")
            .current_dir(seed.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    let gate_repo = tempfile::TempDir::new().unwrap();
    run_git(
        gate_repo.path(),
        &[
            "clone",
            "--bare",
            "--quiet",
            &seed.path().display().to_string(),
            ".",
        ],
    );
    run_git(
        gate_repo.path(),
        &[
            "remote",
            "set-url",
            "origin",
            &origin.path().display().to_string(),
        ],
    );

    let provider = RecordingProvider::new(31);
    let request = OpenDraftRequest {
        bare_repo_path: gate_repo.path(),
        skeleton_commit_sha: &merge_sha,
        branch: "warden/run-merge-leak",
        base_branch: "main",
        intent: "Add a feature",
    };

    let result = open_draft(&request, &provider).await;
    match result {
        Err(GatedError::SkeletonNotContentFree {
            commit_sha, files, ..
        }) => {
            assert_eq!(commit_sha, merge_sha);
            assert_eq!(files, vec!["evil.txt".to_string()]);
        }
        other => panic!("expected SkeletonNotContentFree, got {other:?}"),
    }

    assert!(
        provider.calls().is_empty(),
        "a rejected merge-commit skeleton must never reach the PR provider"
    );

    let ref_check = std::process::Command::new("git")
        .current_dir(origin.path())
        .args(["rev-parse", "--verify", "refs/heads/warden/run-merge-leak"])
        .output()
        .unwrap();
    assert!(
        !ref_check.status.success(),
        "origin must never receive a push when a merge commit in the range \
             introduces content absent from both its parents"
    );
}

#[tokio::test]
async fn open_draft_accepts_an_empty_tree_skeleton_when_base_branch_does_not_exist_on_origin() {
    let origin = tempfile::TempDir::new().unwrap();
    run_git(origin.path(), &["init", "--bare", "--quiet"]);

    let seed = tempfile::TempDir::new().unwrap();
    run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
    run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
    run_git(seed.path(), &["config", "user.name", "warden-test"]);
    run_git(
        seed.path(),
        &["commit", "--quiet", "--allow-empty", "-m", "skeleton"],
    );
    let skeleton_sha = {
        let output = std::process::Command::new("git")
            .current_dir(seed.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    let gate_repo = tempfile::TempDir::new().unwrap();
    run_git(
        gate_repo.path(),
        &[
            "clone",
            "--bare",
            "--quiet",
            &seed.path().display().to_string(),
            ".",
        ],
    );
    run_git(
        gate_repo.path(),
        &[
            "remote",
            "set-url",
            "origin",
            &origin.path().display().to_string(),
        ],
    );

    let provider = RecordingProvider::new(17);
    let request = OpenDraftRequest {
        bare_repo_path: gate_repo.path(),
        skeleton_commit_sha: &skeleton_sha,
        branch: "warden/run-first-ever",
        base_branch: "main",
        intent: "Bootstrap the repo",
    };

    let pr = open_draft(&request, &provider).await.unwrap();
    assert_eq!(pr.number, 17);

    let origin_head = std::process::Command::new("git")
        .current_dir(origin.path())
        .args([
            "log",
            "-1",
            "--format=%H",
            "refs/heads/warden/run-first-ever",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&origin_head.stdout).trim(),
        skeleton_sha
    );
}

#[tokio::test]
async fn open_draft_rejects_a_non_empty_skeleton_when_base_branch_does_not_exist_on_origin() {
    let origin = tempfile::TempDir::new().unwrap();
    run_git(origin.path(), &["init", "--bare", "--quiet"]);

    let seed = tempfile::TempDir::new().unwrap();
    run_git(seed.path(), &["init", "--quiet", "-b", "main"]);
    run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
    run_git(seed.path(), &["config", "user.name", "warden-test"]);
    std::fs::write(seed.path().join("src_business.rs"), "fn business() {}\n").unwrap();
    run_git(seed.path(), &["add", "."]);
    run_git(
        seed.path(),
        &["commit", "--quiet", "-m", "not actually a skeleton"],
    );
    let non_empty_sha = {
        let output = std::process::Command::new("git")
            .current_dir(seed.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    let gate_repo = tempfile::TempDir::new().unwrap();
    run_git(
        gate_repo.path(),
        &[
            "clone",
            "--bare",
            "--quiet",
            &seed.path().display().to_string(),
            ".",
        ],
    );
    run_git(
        gate_repo.path(),
        &[
            "remote",
            "set-url",
            "origin",
            &origin.path().display().to_string(),
        ],
    );

    let provider = RecordingProvider::new(19);
    let request = OpenDraftRequest {
        bare_repo_path: gate_repo.path(),
        skeleton_commit_sha: &non_empty_sha,
        branch: "warden/run-first-ever-rejected",
        base_branch: "main",
        intent: "Bootstrap the repo",
    };

    let result = open_draft(&request, &provider).await;
    match result {
        Err(GatedError::SkeletonNotContentFree {
            commit_sha, files, ..
        }) => {
            assert_eq!(commit_sha, non_empty_sha);
            assert_eq!(files, vec!["src_business.rs".to_string()]);
        }
        other => panic!("expected SkeletonNotContentFree, got {other:?}"),
    }

    assert!(
        provider.calls().is_empty(),
        "a rejected first-run skeleton must never reach the PR provider"
    );
    let ref_check = std::process::Command::new("git")
        .current_dir(origin.path())
        .args([
            "rev-parse",
            "--verify",
            "refs/heads/warden/run-first-ever-rejected",
        ])
        .output()
        .unwrap();
    assert!(
        !ref_check.status.success(),
        "origin must never receive a push for a rejected first-run skeleton"
    );
}

#[tokio::test]
async fn open_draft_rejects_a_blank_intent_before_touching_git_or_the_provider() {
    let (origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
    let provider = RecordingProvider::new(23);

    let request = OpenDraftRequest {
        bare_repo_path: gate_repo.path(),
        skeleton_commit_sha: &commit_sha,
        branch: "warden/run-blank-intent",
        base_branch: "main",
        intent: "   \n  \n",
    };

    let result = open_draft(&request, &provider).await;
    assert!(matches!(result, Err(GatedError::EmptyIntent)));

    assert!(
        provider.calls().is_empty(),
        "a blank intent must be rejected before the PR provider is ever called"
    );
    let ref_check = std::process::Command::new("git")
        .current_dir(origin.path())
        .args([
            "rev-parse",
            "--verify",
            "refs/heads/warden/run-blank-intent",
        ])
        .output()
        .unwrap();
    assert!(
        !ref_check.status.success(),
        "a blank intent must be rejected before anything is pushed to origin"
    );
}

#[tokio::test]
async fn post_cycle_update_only_ever_posts_a_comment() {
    let provider = RecordingProvider::new(1);
    let pr = PrHandle { number: 7 };
    let summary = CycleSummary {
        cycle_number: 2,
        findings: vec![reviewer_finding()],
    };

    post_cycle_update(&pr, &summary, &provider).await.unwrap();

    let calls = provider.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("post_comment(7,"));
}

async fn seeded_run_db(
    state: warden_core::RunState,
    converged_commit_sha: Option<&str>,
) -> (tempfile::TempDir, std::path::PathBuf) {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let write_options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let write_pool = SqlitePoolOptions::new()
        .connect_with(write_options)
        .await
        .unwrap();

    sqlx::query(
        "CREATE TABLE runs (id TEXT PRIMARY KEY, state TEXT NOT NULL, converged_commit_sha TEXT)",
    )
    .execute(&write_pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO runs (id, state, converged_commit_sha) VALUES ('run-1', ?, ?)")
        .bind(state.as_str())
        .bind(converged_commit_sha)
        .execute(&write_pool)
        .await
        .unwrap();
    write_pool.close().await;

    (dir, db_path)
}

#[test]
fn finalize_pr_body_leaves_summary_body_untouched_when_there_is_no_evidence() {
    let pr = PrHandle { number: 1 };
    let request = FinalizeRequest {
        bare_repo_path: Path::new("/unused"),
        branch: "main",
        run_id: "run-1",
        pushed_commit_sha: "deadbeef",
        pr: &pr,
        summary_body: "full run summary",
        evidence: &[],
        repo_slug: "acme/widgets",
    };
    assert_eq!(finalize_pr_body(&request), "full run summary");
}

#[test]
fn finalize_pr_body_appends_an_evidence_section_via_warden_core_format_evidence_section() {
    let pr = PrHandle { number: 1 };
    let evidence = [warden_core::EvidenceRow {
        cycle_number: 1,
        evidence_type: warden_core::EvidenceType::Image,
        repo_relative_path: ".warden/evidence/1/screenshot.png".to_string(),
        description: "login screen".to_string(),
    }];
    let request = FinalizeRequest {
        bare_repo_path: Path::new("/unused"),
        branch: "main",
        run_id: "run-1",
        pushed_commit_sha: "deadbeef",
        pr: &pr,
        summary_body: "full run summary",
        evidence: &evidence,
        repo_slug: "acme/widgets",
    };

    let body = finalize_pr_body(&request);

    assert!(body.starts_with("full run summary"));
    assert_eq!(
        body,
        format!(
            "full run summary\n\n{}",
            warden_core::format_evidence_section(&evidence, "acme/widgets", "main")
        ),
        "finalize_pr_body must delegate to warden_core::format_evidence_section, not a local copy"
    );
}

#[tokio::test]
async fn finalize_pushes_and_marks_ready_when_converged_and_hash_matches() {
    let (_origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
    let (_db_dir, db_path) =
        seeded_run_db(warden_core::RunState::Converged, Some(&commit_sha)).await;
    let pool = crate::db::connect_read_only(&db_path).await.unwrap();
    let provider = RecordingProvider::new(1);
    let pr = PrHandle { number: 7 };

    let request = FinalizeRequest {
        bare_repo_path: gate_repo.path(),
        branch: "main",
        run_id: "run-1",
        pushed_commit_sha: &commit_sha,
        pr: &pr,
        summary_body: "full run summary",
        evidence: &[],
        repo_slug: "acme/widgets",
    };
    let outcome = finalize(&pool, &request, &provider).await.unwrap();

    assert_eq!(
        outcome,
        FinalizeOutcome::Finalized {
            commit_sha: commit_sha.clone()
        }
    );

    let calls = provider.calls();
    assert_eq!(
        calls,
        vec![
            "update_body(7, full run summary)".to_string(),
            "mark_ready(7)".to_string(),
        ]
    );
}

#[tokio::test]
async fn finalize_posts_an_evidence_section_when_evidence_was_captured() {
    let (_origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
    let (_db_dir, db_path) =
        seeded_run_db(warden_core::RunState::Converged, Some(&commit_sha)).await;
    let pool = crate::db::connect_read_only(&db_path).await.unwrap();
    let provider = RecordingProvider::new(1);
    let pr = PrHandle { number: 7 };
    let evidence = [warden_core::EvidenceRow {
        cycle_number: 1,
        evidence_type: warden_core::EvidenceType::Image,
        repo_relative_path: ".warden/evidence/1/screenshot.png".to_string(),
        description: "login screen".to_string(),
    }];

    let request = FinalizeRequest {
        bare_repo_path: gate_repo.path(),
        branch: "main",
        run_id: "run-1",
        pushed_commit_sha: &commit_sha,
        pr: &pr,
        summary_body: "full run summary",
        evidence: &evidence,
        repo_slug: "acme/widgets",
    };
    finalize(&pool, &request, &provider).await.unwrap();

    let calls = provider.calls();
    assert!(
        calls[0].contains("## Evidence"),
        "finalize must post an Evidence section when evidence was captured: {calls:?}"
    );
    assert!(
        calls[0].contains(
            "https://raw.githubusercontent.com/acme/widgets/main/.warden/evidence/1/screenshot.png"
        ),
        "calls were: {calls:?}"
    );
}

#[tokio::test]
async fn finalize_blocks_and_never_touches_the_provider_when_not_converged() {
    let (_origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
    let (_db_dir, db_path) = seeded_run_db(warden_core::RunState::RunningStep(0), None).await;
    let pool = crate::db::connect_read_only(&db_path).await.unwrap();
    let provider = RecordingProvider::new(1);
    let pr = PrHandle { number: 7 };

    let request = FinalizeRequest {
        bare_repo_path: gate_repo.path(),
        branch: "main",
        run_id: "run-1",
        pushed_commit_sha: &commit_sha,
        pr: &pr,
        summary_body: "full run summary",
        evidence: &[],
        repo_slug: "acme/widgets",
    };
    let outcome = finalize(&pool, &request, &provider).await.unwrap();

    assert_eq!(
        outcome,
        FinalizeOutcome::Blocked(GateBlockReason::NotConverged {
            actual_state: warden_core::RunState::RunningStep(0)
        })
    );
    assert!(
        provider.calls().is_empty(),
        "a blocked finalize must never call the provider"
    );
}

#[tokio::test]
async fn finalize_blocks_and_never_touches_the_provider_on_hash_mismatch() {
    let (origin, _seed, gate_repo, commit_sha) = gate_repo_fixture();
    let (_db_dir, db_path) = seeded_run_db(
        warden_core::RunState::Converged,
        Some("some-other-validated-sha"),
    )
    .await;
    let pool = crate::db::connect_read_only(&db_path).await.unwrap();
    let provider = RecordingProvider::new(1);
    let pr = PrHandle { number: 7 };

    let request = FinalizeRequest {
        bare_repo_path: gate_repo.path(),
        branch: "main",
        run_id: "run-1",
        pushed_commit_sha: &commit_sha,
        pr: &pr,
        summary_body: "full run summary",
        evidence: &[],
        repo_slug: "acme/widgets",
    };
    let outcome = finalize(&pool, &request, &provider).await.unwrap();

    assert_eq!(
        outcome,
        FinalizeOutcome::Blocked(GateBlockReason::HashMismatch {
            validated: Some("some-other-validated-sha".to_string()),
            pushed: commit_sha.clone(),
        })
    );
    assert!(
        provider.calls().is_empty(),
        "a blocked finalize (hash mismatch) must never call the provider"
    );

    let origin_head = std::process::Command::new("git")
        .current_dir(origin.path())
        .args(["log", "-1", "--format=%H", "refs/heads/main"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&origin_head.stdout).trim(),
        commit_sha,
        "a blocked finalize must never push anything to origin"
    );
}

struct StatefulProvider {
    next_pr_number: u64,
    draft: std::sync::Mutex<bool>,
    body: std::sync::Mutex<String>,
}

impl StatefulProvider {
    fn new(next_pr_number: u64) -> Self {
        Self {
            next_pr_number,
            draft: std::sync::Mutex::new(true),
            body: std::sync::Mutex::new(String::new()),
        }
    }

    fn is_draft(&self) -> bool {
        *self.draft.lock().unwrap()
    }

    fn body(&self) -> String {
        self.body.lock().unwrap().clone()
    }
}

impl PrProvider for StatefulProvider {
    async fn open_draft(&self, params: &OpenDraftParams<'_>) -> Result<PrHandle> {
        *self.body.lock().unwrap() = params.body.to_string();
        Ok(PrHandle {
            number: self.next_pr_number,
        })
    }

    async fn post_comment(&self, _pr: &PrHandle, _body: &str) -> Result<()> {
        Ok(())
    }

    async fn mark_ready(&self, _pr: &PrHandle) -> Result<()> {
        *self.draft.lock().unwrap() = false;
        Ok(())
    }

    async fn update_body(&self, _pr: &PrHandle, body: &str) -> Result<()> {
        *self.body.lock().unwrap() = body.to_string();
        Ok(())
    }
}

#[tokio::test]
async fn pr_never_leaves_draft_or_changes_body_before_finalize_across_a_full_cycle_sequence() {
    let (_origin, _seed, gate_repo, skeleton_sha) = gate_repo_fixture();
    let provider = StatefulProvider::new(21);

    let open_request = OpenDraftRequest {
        bare_repo_path: gate_repo.path(),
        skeleton_commit_sha: &skeleton_sha,
        branch: "warden/run-4",
        base_branch: "main",
        intent: "Add JWT expiry handling",
    };
    let pr = open_draft(&open_request, &provider).await.unwrap();
    assert!(
        provider.is_draft(),
        "PR must be a draft immediately after OpenDraft"
    );
    let body_after_open = provider.body();

    for cycle in 1..=2 {
        let summary = CycleSummary {
            cycle_number: cycle,
            findings: vec![reviewer_finding()],
        };
        post_cycle_update(&pr, &summary, &provider).await.unwrap();
        assert!(
            provider.is_draft(),
            "PostCycleUpdate must never flip a PR out of draft (cycle {cycle})"
        );
        assert_eq!(
            provider.body(),
            body_after_open,
            "PostCycleUpdate must never touch the PR body (cycle {cycle})"
        );
    }

    let (_db_dir, db_path) =
        seeded_run_db(warden_core::RunState::Converged, Some(&skeleton_sha)).await;
    let pool = crate::db::connect_read_only(&db_path).await.unwrap();
    let finalize_request = FinalizeRequest {
        bare_repo_path: gate_repo.path(),
        branch: "warden/run-4",
        run_id: "run-1",
        pushed_commit_sha: &skeleton_sha,
        pr: &pr,
        summary_body: "full run summary",
        evidence: &[],
        repo_slug: "acme/widgets",
    };
    let outcome = finalize(&pool, &finalize_request, &provider).await.unwrap();

    assert_eq!(
        outcome,
        FinalizeOutcome::Finalized {
            commit_sha: skeleton_sha.clone()
        }
    );
    assert!(
        !provider.is_draft(),
        "Finalize must be the point where the PR leaves draft"
    );
    assert_eq!(
        provider.body(),
        "full run summary",
        "Finalize must update the PR body to the full run summary"
    );
}
