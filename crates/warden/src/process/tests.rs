use super::*;
use std::path::PathBuf;
use tempfile::TempDir;

// -----------------------------------------------------------------
// `validate_agent_program` (issue #26, belt-and-braces)
// -----------------------------------------------------------------

/// A dedicated `<run_worktrees_root>/<role>` layout, mirroring what
/// `WorktreeManager::create` actually produces
/// (`<warden_home>/worktrees/<run_id>/<role>`) -- used by every test
/// below instead of an unrelated bare `TempDir` for `worktree_path`, so
/// the MEDIUM (issue #26 review) coverage of *other* roles' worktrees
/// under the same `run_worktrees_root` has something real to check.
struct WorktreeLayout {
    run_worktrees_root: TempDir,
}

impl WorktreeLayout {
    fn new() -> Self {
        Self {
            run_worktrees_root: TempDir::new().unwrap(),
        }
    }

    fn role_worktree(&self, role: &str) -> PathBuf {
        let path = self.run_worktrees_root.path().join(role);
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}

#[test]
fn a_bare_program_name_with_no_separator_is_always_allowed_for_reviewer_and_tester() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();
    for role in ["reviewer", "tester"] {
        assert!(validate_agent_program(
            role,
            false,
            "claude",
            &[],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .is_ok());
    }
}

#[test]
fn a_relative_path_is_refused_for_reviewer_and_tester() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();
    for role in ["reviewer", "tester"] {
        let error = validate_agent_program(
            role,
            false,
            "./reviewer.sh",
            &[],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
        assert!(error.to_string().contains("./reviewer.sh"), "{error}");
    }
}

#[test]
fn an_absolute_path_inside_the_role_worktree_is_refused() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();
    let program = worktree.join("reviewer.sh");
    std::fs::write(&program, "#!/bin/sh\n").unwrap();

    let error = validate_agent_program(
        "reviewer",
        false,
        program.to_str().unwrap(),
        &[],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
}

#[test]
fn an_absolute_path_inside_the_run_base_repo_is_refused() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("tester");
    let repo = TempDir::new().unwrap();
    let program = repo.path().join(".warden/agents/reviewer.sh");
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    std::fs::write(&program, "#!/bin/sh\n").unwrap();

    let error = validate_agent_program(
        "tester",
        false,
        program.to_str().unwrap(),
        &[],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
}

/// Issue #26 review, MEDIUM: the original guard only checked the
/// checked role's own worktree and the base repo -- leaving the
/// *coder's* own worktree, under the same `run_worktrees_root`, entirely
/// unchecked even though it is the most coder-controllable directory on
/// disk (the coder runs with `Bash` there and writes freely, including
/// files it never commits). A reviewer `program` naming a script under
/// the coder's worktree must now be refused too.
#[test]
fn an_absolute_path_inside_the_coders_own_worktree_for_this_run_is_refused() {
    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let coder_worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
    let program = coder_worktree.join("tool.sh");
    std::fs::write(&program, "#!/bin/sh\n").unwrap();

    let error = validate_agent_program(
        "reviewer",
        false,
        program.to_str().unwrap(),
        &[],
        &reviewer_worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
    assert!(error.to_string().contains("run's own worktrees"), "{error}");
}

#[test]
fn an_absolute_path_outside_the_worktree_the_repo_and_the_run_worktrees_root_is_allowed() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let program = elsewhere.path().join("some-tool");
    std::fs::write(&program, "#!/bin/sh\n").unwrap();

    assert!(validate_agent_program(
        "reviewer",
        false,
        program.to_str().unwrap(),
        &[],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .is_ok());
}

/// The whole point of this guard: it must never apply to the producer
/// step (the coder in the built-in default workflow), which already has
/// full repo access and is the repo's own untrusted step in the first
/// place -- even a program that would be refused for a gated step must
/// pass unchanged for the producer.
#[test]
fn the_producer_step_is_never_subject_to_this_guard() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
    let program = repo.path().join(".warden/agents/coder.sh");
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    std::fs::write(&program, "#!/bin/sh\n").unwrap();

    assert!(validate_agent_program(
        "coder",
        true,
        program.to_str().unwrap(),
        &[],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .is_ok());
    assert!(validate_agent_program(
        "coder",
        true,
        "./coder.sh",
        &[],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .is_ok());
}

/// A `program` that doesn't exist on disk at all must still be checked
/// against the containment rule (via `canonicalize_best_effort`'s
/// ancestor walk), not silently allowed just because it can't be
/// canonicalized outright.
#[test]
fn a_nonexistent_absolute_path_inside_the_worktree_is_still_refused() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();
    let program = worktree.join("does-not-exist-yet.sh");

    let error = validate_agent_program(
        "reviewer",
        false,
        program.to_str().unwrap(),
        &[],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
}

// -----------------------------------------------------------------
// `validate_agent_program`, `args` coverage (issue #59)
// -----------------------------------------------------------------

#[test]
fn a_relative_path_arg_is_refused_for_reviewer_and_tester() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();
    for role in ["reviewer", "tester"] {
        let error = validate_agent_program(
            role,
            false,
            "claude",
            &["--wrapper".to_string(), "./reviewer.sh".to_string()],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
        assert!(error.to_string().contains("./reviewer.sh"), "{error}");
    }
}

#[test]
fn an_absolute_arg_inside_the_role_worktree_is_refused() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();
    let wrapper = worktree.join("reviewer.sh");
    std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &[
            "--wrapper".to_string(),
            wrapper.to_str().unwrap().to_string(),
        ],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
}

#[test]
fn an_absolute_arg_inside_the_run_base_repo_is_refused() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("tester");
    let repo = TempDir::new().unwrap();
    let wrapper = repo.path().join(".warden/agents/reviewer.sh");
    std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

    let error = validate_agent_program(
        "tester",
        false,
        "claude",
        &[
            "--wrapper".to_string(),
            wrapper.to_str().unwrap().to_string(),
        ],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
}

/// Mirrors the `program` coverage (issue #26 review, MEDIUM): an `args`
/// entry resolving inside *another* role's worktree for this run --
/// most importantly the coder's own -- must be refused too, not just
/// the checked role's own worktree.
#[test]
fn an_absolute_arg_inside_the_coders_own_worktree_for_this_run_is_refused() {
    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let coder_worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
    let wrapper = coder_worktree.join("tool.sh");
    std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &[
            "--wrapper".to_string(),
            wrapper.to_str().unwrap().to_string(),
        ],
        &reviewer_worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    assert!(error.to_string().contains("run's own worktrees"), "{error}");
}

#[test]
fn an_absolute_arg_outside_the_worktree_the_repo_and_the_run_worktrees_root_is_allowed() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let wrapper = elsewhere.path().join("some-tool");
    std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

    assert!(validate_agent_program(
        "reviewer",
        false,
        "claude",
        &[
            "--wrapper".to_string(),
            wrapper.to_str().unwrap().to_string()
        ],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .is_ok());
}

/// The exact false positive the issue calls out by name: an ordinary
/// `--flag value` pair (no path separator anywhere) must never be
/// treated as path-like.
#[test]
fn an_ordinary_non_path_arg_is_allowed() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();

    assert!(validate_agent_program(
        "reviewer",
        false,
        "claude",
        &["--model".to_string(), "sonnet".to_string()],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .is_ok());
}

/// The other false positive the issue calls out by name: a URL contains
/// a path separator (`://`) but is not a filesystem path.
#[test]
fn a_url_arg_is_allowed() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();

    assert!(validate_agent_program(
        "reviewer",
        false,
        "claude",
        &[
            "--endpoint".to_string(),
            "https://example.com/reviewer.sh".to_string()
        ],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .is_ok());
}

/// The false positive verified live against this codebase's own shipped
/// adapters: every one of them passes its role's entire system prompt as
/// a single argv entry, and all three built-in default prompts contain
/// at least one `/` -- without the whitespace exception, this would
/// refuse every reviewer/tester invocation using a shipped adapter's
/// default prompt.
#[test]
fn a_multi_word_value_containing_a_separator_is_allowed() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();

    assert!(validate_agent_program(
        "reviewer",
        false,
        "claude",
        &[
            "--append-system-prompt".to_string(),
            "issues a prior reviewer/tester/CI raised".to_string()
        ],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .is_ok());
}

/// `--flag=value` packs the flag and its value into one argv entry --
/// the value after the first `=` must still be checked.
#[test]
fn a_flag_equals_relative_path_form_is_refused() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &["--wrapper=./reviewer.sh".to_string()],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    assert!(error.to_string().contains("./reviewer.sh"), "{error}");
}

/// The `args` check is subject to the exact same `is_producer` exemption
/// as `program` -- the coder must never be refused an argument that
/// would be refused for a gated step.
#[test]
fn the_producer_step_is_never_subject_to_the_args_guard() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();

    assert!(validate_agent_program(
        "coder",
        true,
        "claude",
        &["--wrapper".to_string(), "./coder.sh".to_string()],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .is_ok());
}

/// End-to-end regression for the false positive this guard must never
/// reintroduce (issue #59): every shipped `ToolAdapter`'s *real*
/// `build_command`, fed its own default reviewer/tester prompt (not a
/// hand-picked string), must still pass `validate_agent_program`. This
/// is what actually caught the system-prompt false positive during
/// implementation -- `path_like_candidate`'s whitespace exception exists
/// because this test failed without it.
#[test]
fn every_shipped_adapters_default_command_for_reviewer_and_tester_passes_the_guard() {
    use crate::tool_adapter::{ClaudeAdapter, CodexAdapter, MistralAdapter, ToolAdapter};
    use warden_core::{AgentDefinition, AgentRole};

    let layout = WorktreeLayout::new();
    let repo = TempDir::new().unwrap();

    fn check(
        adapter: &impl ToolAdapter,
        role: AgentRole,
        role_name: &str,
        layout: &WorktreeLayout,
        repo: &TempDir,
    ) {
        let worktree = layout.role_worktree(role_name);
        let definition = AgentDefinition::new(
            None,
            None,
            adapter.default_tools(role).map(str::to_string),
            None,
            adapter.default_prompt(role),
        )
        .unwrap();
        let command = adapter.build_command(&definition).unwrap();

        assert!(
            validate_agent_program(
                role_name,
                false,
                &command.program,
                &command.args,
                &worktree,
                repo.path(),
                layout.run_worktrees_root.path(),
                &[],
            )
            .is_ok(),
            "{role_name} via {} was refused for its own default command: {:?}",
            std::any::type_name_of_val(adapter),
            command.args
        );
    }

    for (role, role_name) in [
        (AgentRole::Reviewer, "reviewer"),
        (AgentRole::Tester, "tester"),
    ] {
        check(&ClaudeAdapter, role, role_name, &layout, &repo);
        check(&CodexAdapter, role, role_name, &layout, &repo);
        check(&MistralAdapter, role, role_name, &layout, &repo);
    }
}

// -----------------------------------------------------------------
// `path_like_candidate` heuristic hardening (issue #59 review)
// -----------------------------------------------------------------

/// Issue #59 review, HIGH: a whitespace-containing value that is
/// otherwise unambiguous evidence of a path (absolute, inside the
/// coder's own worktree) must still be refused -- POSIX paths can
/// contain spaces, and the coder can write a file literally named
/// `my tool.sh` inside its own worktree via its `Bash` grant. Before
/// the fix this was a one-character bypass of the whole guard.
#[test]
fn an_absolute_arg_with_a_space_inside_the_coders_worktree_is_still_refused() {
    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let coder_worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
    let wrapper = coder_worktree.join("my tool.sh");
    std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &[
            "--wrapper".to_string(),
            wrapper.to_str().unwrap().to_string(),
        ],
        &reviewer_worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
}

/// Issue #59 review, HIGH: the relative-path counterpart -- an
/// unambiguous `./`-prefixed value with a space in it must be refused
/// exactly like one without a space.
#[test]
fn a_relative_arg_with_a_space_is_still_refused() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &["--wrapper".to_string(), "./sub dir/tool.sh".to_string()],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    assert!(error.to_string().contains("./sub dir/tool.sh"), "{error}");
}

/// Issue #59 review, MEDIUM 2: a `file://` URI names a real filesystem
/// path and must be resolved and refused exactly like the equivalent
/// bare absolute path -- it must never be laundered through the
/// URL-scheme exemption just because it looks like a URL.
#[test]
fn a_file_url_onto_the_coders_worktree_is_refused() {
    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let coder_worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
    let wrapper = coder_worktree.join("tool.sh");
    std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();
    let file_url = format!("file://{}", wrapper.to_str().unwrap());

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &["--wrapper".to_string(), file_url],
        &reviewer_worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
}

/// Issue #59 review, MEDIUM 2: an invented scheme in front of a
/// relative path must not be treated as a URL -- `://` alone is not
/// evidence of a genuine network-protocol URL (`has_url_scheme`'s
/// previous, over-broad RFC 3986 grammar check accepted any
/// syntactically valid scheme, including one the coder can `mkdir`
/// literally as a directory name, e.g. `sh:`).
#[test]
fn an_invented_scheme_in_front_of_a_relative_path_does_not_bypass_the_guard() {
    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let coder_worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
    // Mirrors the coder's own worktree in the arg text, the same way a
    // real relative-path wrapper would resolve against it -- the exact
    // value doesn't need to exist on disk for the relative-path branch.
    let arg = format!(
        "sh://../{}/tool.sh",
        coder_worktree.file_name().unwrap().to_str().unwrap()
    );

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &["--wrapper".to_string(), arg],
        &reviewer_worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
}

/// A genuine network-protocol URL (on the allowlist) must still be
/// allowed -- the fix for finding 2 must not regress the original
/// false-positive fix it's built on top of.
#[test]
fn a_genuine_https_url_is_still_allowed() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();

    assert!(validate_agent_program(
        "reviewer",
        false,
        "claude",
        &[
            "--endpoint".to_string(),
            "https://example.com/reviewer.sh".to_string()
        ],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .is_ok());
}

/// Independent verification (issue #59 QA pass): a symlink whose *link
/// text* lives well outside every forbidden root, but that resolves to a
/// real file inside the coder's own worktree, must still be refused --
/// the coder can create a symlink anywhere it has write access (its own
/// worktree, e.g. via a checked-in symlink or something a build step
/// produces) pointing back at a script it controls; if the guard only
/// compared the *lexical* candidate string against the forbidden roots,
/// a symlink like this would sail through. `canonicalize_best_effort`
/// resolving via `std::fs::canonicalize` (which follows symlinks) is
/// what closes this -- this pins that behaviour down as a guarantee, not
/// an incidental property.
#[cfg(unix)]
#[test]
fn a_symlink_outside_every_forbidden_root_that_resolves_into_the_coders_worktree_is_refused() {
    use std::os::unix::fs::symlink;

    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let coder_worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
    let real_target = coder_worktree.join("real-tool.sh");
    std::fs::write(&real_target, "#!/bin/sh\n").unwrap();
    // The symlink itself lives in a directory unrelated to any of the
    // three forbidden roots -- only its *target* is coder-controlled.
    let elsewhere = TempDir::new().unwrap();
    let link = elsewhere.path().join("innocuous-name.sh");
    symlink(&real_target, &link).unwrap();

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &["--wrapper".to_string(), link.to_str().unwrap().to_string()],
        &reviewer_worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
}

/// The mirror image, and not a security concern: a symlink that lives
/// *inside* the role's own worktree but resolves to a real file genuinely
/// outside every forbidden root must still be allowed -- exactly as a
/// bare absolute path to that same external location already is
/// (`an_absolute_arg_outside_the_worktree_the_repo_and_the_run_worktrees_root_is_allowed`).
/// This exists to pin down that the containment check compares the
/// symlink's *resolved* location, not merely whether its literal path
/// string happens to start with `worktree_path` -- a lexical-only
/// comparison would wrongly refuse this.
#[cfg(unix)]
#[test]
fn a_symlink_inside_the_worktree_that_resolves_outside_every_forbidden_root_is_allowed() {
    use std::os::unix::fs::symlink;

    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let real_target = elsewhere.path().join("legit-tool.sh");
    std::fs::write(&real_target, "#!/bin/sh\n").unwrap();
    let link = reviewer_worktree.join("link-out.sh");
    symlink(&real_target, &link).unwrap();

    assert!(validate_agent_program(
        "reviewer",
        false,
        "claude",
        &["--wrapper".to_string(), link.to_str().unwrap().to_string(),],
        &reviewer_worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .is_ok());
}

/// Independent verification (issue #59 QA pass): `..` traversal must be
/// resolved before the containment check runs, not compared lexically --
/// an absolute candidate whose *literal* prefix names the reviewer's own
/// (already-permitted) worktree, but that walks back out via `..` into
/// the coder's, must still be refused. Every segment here genuinely
/// exists on disk, so `canonicalize_best_effort` resolves the whole
/// thing via a single real `std::fs::canonicalize` call (the OS's own
/// `..` handling), not the best-effort ancestor-walking fallback --
/// see `path_util::canonicalize_best_effort`'s own docs for why a
/// not-yet-existing path is a materially different (and, for this
/// guard's purposes, inert -- see this crate's own test-derived
/// findings) case.
#[test]
fn a_dotdot_traversal_from_the_role_worktree_into_the_coders_worktree_is_refused() {
    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let coder_worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
    let tool = coder_worktree.join("tool.sh");
    std::fs::write(&tool, "#!/bin/sh\n").unwrap();
    let traversal_arg = format!("{}/../coder/tool.sh", reviewer_worktree.display());

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &["--wrapper".to_string(), traversal_arg],
        &reviewer_worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
}

/// Independent verification (issue #59 QA pass): [`strip_file_scheme`]'s
/// own docs claim case-insensitivity (`FILE://`) but no existing test
/// actually exercised anything but a lowercase `file://` -- a mixed-case
/// scheme must not launder a coder-controlled path past the
/// containment check.
#[test]
fn a_mixed_case_file_scheme_onto_the_coders_worktree_is_refused() {
    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let coder_worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
    let tool = coder_worktree.join("tool.sh");
    std::fs::write(&tool, "#!/bin/sh\n").unwrap();
    let arg = format!("FiLe://{}", tool.to_str().unwrap());

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &["--wrapper".to_string(), arg],
        &reviewer_worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
}

/// **Independent verification, real defect found (issue #59 QA pass):**
/// `path_like_candidate`'s weak tier exempts *any* whitespace-containing
/// value unconditionally -- but the "strong evidence" tier that survives
/// whitespace only recognises `./`, `../`, `~`, or an absolute prefix
/// (see that function's own docs). A **relative** path that contains a
/// separator *and* whitespace but does not start with `./`/`../`/`~`
/// (e.g. `agents/evil script.sh`, mirroring `./sub dir/tool.sh` from
/// `a_relative_arg_with_a_space_is_still_refused` minus only the leading
/// `./`) falls into neither tier's protection: it is weak evidence
/// (has a separator, no unambiguous prefix) *and* contains whitespace,
/// so [`path_like_candidate`] returns `None` for it and it is never
/// containment-checked at all -- even though it is exactly the same
/// coder-controlled-relative-path hazard `check_containment`'s own docs
/// describe ("resolves against `worktree_path`, the role's own
/// worktree... which the coder can write to"). The coder can create a
/// file with a literal space in its own worktree via its `Bash` grant
/// (the same premise the whitespace exemption itself relies on), so a
/// future adapter emitting `--wrapper agents/evil script.sh` as a single
/// argv value would defeat this guard exactly as issue #59 set out to
/// prevent -- it merely needs to omit the `./` prefix.
///
/// This assertion is what issue #59's intent actually demands (refusal);
/// it currently fails against `path_like_candidate`
/// (`crates/warden/src/process.rs`), which returns `Some`/`None` on
/// whitespace alone regardless of whether the value is a bare relative
/// path shape. Left failing deliberately (code-standards.md: never
/// weaken a test to make it pass) -- see the QA report for the fix this
/// is the acceptance criterion for.
#[test]
fn a_relative_path_with_a_separator_and_whitespace_but_no_dot_prefix_is_refused() {
    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let coder_worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
    std::fs::create_dir_all(coder_worktree.join("agents")).unwrap();
    std::fs::write(
        coder_worktree.join("agents").join("evil script.sh"),
        "#!/bin/sh\n",
    )
    .unwrap();

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &["--wrapper".to_string(), "agents/evil script.sh".to_string()],
        &reviewer_worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
}

/// Pins the real false positive found while fixing the test above
/// (issue #59 review round 2): a shell-script `args` value whose first
/// whitespace-delimited token is a quoted assignment embedding an
/// absolute path (`dir='<temp path>'`, exactly the shape
/// `orchestrator::convergence`'s own `sh -c` test fixtures construct)
/// must not be refused just because that token contains a `/` -- it is
/// shell syntax, not a bare relative path.
#[test]
fn a_shell_assignment_embedding_an_absolute_path_as_its_first_token_is_allowed() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("tester");
    let repo = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let script = format!(
            "\n                dir='{}'\n                n=$(cat \"$dir/count\" 2>/dev/null || echo 0)\n                echo \"$n\" > \"$dir/count\"\n                ",
            elsewhere.path().display()
        );

    assert!(validate_agent_program(
        "tester",
        false,
        "sh",
        &["-c".to_string(), script],
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .is_ok());
}

/// The shell-metacharacter carve-out must not become the same
/// one-character bypass the whitespace exemption was: the coder names
/// the files in its own worktree, so if an odd character in the
/// *filename* exempted the value, evading the guard would cost it one
/// keystroke. `agents/evil.sh` and `agents/evil$.sh` must be refused
/// alike -- only shell syntax *before* the first separator exempts.
#[test]
fn a_metacharacter_after_the_first_separator_does_not_exempt_a_relative_path() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();

    for name in [
        "agents/evil$.sh",
        "agents/evil(1).sh",
        "agents/evil;.sh",
        "agents/evil*.sh",
        "agents/evil script.sh",
    ] {
        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &["--wrapper".to_string(), name.to_string()],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .expect_err("a relative wrapper path must be refused whatever its filename contains");

        assert!(
            matches!(error, ProcessError::UntrustedAgentArg { ref arg, .. } if arg == name),
            "expected UntrustedAgentArg naming {name}, got {error:?}"
        );
    }
}

// -----------------------------------------------------------------
// `trusted_arg_values` escape hatch (issue #59 review, MEDIUM 4)
// -----------------------------------------------------------------

/// The concrete false positive the review demonstrated: a
/// vendor-prefixed model id looks exactly like a relative path to the
/// separator-based heuristic. Refused without the hatch, allowed once
/// the caller vouches for that exact value.
#[test]
fn a_vendor_prefixed_model_value_is_refused_without_the_hatch_and_allowed_with_it() {
    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();
    let args = vec!["--model".to_string(), "anthropic/claude-3-opus".to_string()];

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &args,
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));

    assert!(validate_agent_program(
        "reviewer",
        false,
        "claude",
        &args,
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &["anthropic/claude-3-opus".to_string()],
    )
    .is_ok());
}

/// End-to-end regression using the *real* `ClaudeAdapter::build_command`
/// (issue #59 review, MEDIUM 4's own ask): `model:
/// mistralai/mistral-large` in a reviewer's `AgentDefinition` must work
/// once the caller vouches for it, exactly as it would coming from
/// `orchestrator::mod::trusted_arg_values_for_step`.
#[test]
fn a_vendor_prefixed_model_from_a_real_adapter_command_works_with_the_hatch() {
    use crate::tool_adapter::{ClaudeAdapter, ToolAdapter};
    use warden_core::AgentDefinition;

    let layout = WorktreeLayout::new();
    let worktree = layout.role_worktree("reviewer");
    let repo = TempDir::new().unwrap();
    let definition = AgentDefinition::new(
        None,
        None,
        None,
        Some("mistralai/mistral-large".to_string()),
        "be a reviewer",
    )
    .unwrap();
    let command = ClaudeAdapter.build_command(&definition).unwrap();

    let error = validate_agent_program(
        "reviewer",
        false,
        &command.program,
        &command.args,
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));

    assert!(validate_agent_program(
        "reviewer",
        false,
        &command.program,
        &command.args,
        &worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        &["mistralai/mistral-large".to_string()],
    )
    .is_ok());
}

/// Issue #59 review, MEDIUM 4's own explicit ask: vouching for one
/// literal value must never smuggle a *different*, genuinely
/// coder-controlled path through. `trusted_arg_values` is compared by
/// exact value equality only -- an unrelated trusted entry must have no
/// effect on an actual containment violation.
#[test]
fn a_trusted_value_does_not_smuggle_an_unrelated_coder_controlled_path() {
    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let coder_worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
    let wrapper = coder_worktree.join("tool.sh");
    std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

    let error = validate_agent_program(
        "reviewer",
        false,
        "claude",
        &[
            "--wrapper".to_string(),
            wrapper.to_str().unwrap().to_string(),
        ],
        &reviewer_worktree,
        repo.path(),
        layout.run_worktrees_root.path(),
        // Vouches for an unrelated model string -- must not affect the
        // unlisted, actually-malicious `--wrapper` value above.
        &["anthropic/claude-3-opus".to_string()],
    )
    .unwrap_err();
    assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
}

#[tokio::test]
async fn captures_stdout_and_exit_code_of_a_successful_command() {
    let dir = TempDir::new().unwrap();
    let cmd = AgentCommand::new("sh", ["-c", "echo hello"]);
    let outcome = spawn_and_wait(&cmd, dir.path(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout.trim(), "hello");
}

#[tokio::test]
async fn reports_a_non_zero_exit_code_as_a_normal_outcome_not_an_error() {
    let dir = TempDir::new().unwrap();
    let cmd = AgentCommand::new("sh", ["-c", "exit 7"]);
    let outcome = spawn_and_wait(&cmd, dir.path(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome.exit_code, 7);
}

#[tokio::test]
async fn spawn_exposes_the_pid_before_completion() {
    let dir = TempDir::new().unwrap();
    let cmd = AgentCommand::new("sh", ["-c", "sleep 0.2"]);
    let child = spawn(&cmd, dir.path()).unwrap();
    let pid = child
        .id()
        .expect("pid available for a freshly spawned child");
    let start_time = process_start_time(pid).expect("start time available for a live process");
    assert!(is_process_alive(pid, start_time));
    wait(child, "sh", None, CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn cancellation_kills_the_child_and_returns_cancelled_error() {
    let dir = TempDir::new().unwrap();
    let cmd = AgentCommand::new("sh", ["-c", "sleep 30"]);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move { spawn_and_wait(&cmd, dir.path(), cancel_clone).await });
    cancel.cancel();

    let result = handle.await.unwrap();
    assert!(matches!(result, Err(ProcessError::Cancelled { .. })));
}

/// ADR-0012 (issue #20 Scope B): a payload written to stdin must reach
/// the child, and the write half must be closed afterwards so a child
/// that reads until EOF (`cat` with no arguments) actually sees one and
/// exits, rather than hanging forever waiting for more input.
#[tokio::test]
async fn stdin_payload_is_written_and_closed_so_the_child_sees_it_and_exits() {
    let dir = TempDir::new().unwrap();
    let cmd = AgentCommand::new("cat", Vec::<String>::new());
    let child = spawn(&cmd, dir.path()).unwrap();
    let outcome = wait(
        child,
        "cat",
        Some("hello from warden".to_string()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, "hello from warden");
}

/// ADR-0012 regression test: writing a large stdin payload while the
/// child also produces enough stdout to fill an OS pipe buffer *before*
/// it finishes reading stdin must not deadlock. Sequenced deliberately
/// (write >64KiB of stdout first, only then drain stdin) so a naive
/// "write the whole payload, then read stdout" implementation would
/// hang: the child blocks on its own full stdout pipe (nobody's
/// draining it yet) while we block on the child's full stdin pipe (it
/// isn't reading yet either). Bounded by a timeout so a regression fails
/// the test instead of hanging the suite.
#[tokio::test]
async fn writing_a_large_stdin_payload_does_not_deadlock_on_large_stdout() {
    let dir = TempDir::new().unwrap();
    // Emits 200_000 bytes of stdout first (well past a typical 64KiB
    // pipe buffer), then only afterwards drains stdin to completion.
    let cmd = AgentCommand::new(
        "sh",
        ["-c", "head -c 200000 /dev/zero; cat > /dev/null; exit 0"],
    );
    let child = spawn(&cmd, dir.path()).unwrap();
    let large_payload = "x".repeat(200_000);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        wait(child, "sh", Some(large_payload), CancellationToken::new()),
    )
    .await
    .expect("wait must not hang when both stdin and stdout exceed the pipe buffer size");

    assert_eq!(result.unwrap().exit_code, 0);
}

// Issue #50 review, MEDIUM 3: the `on_stdout_line` callback tests that
// used to live here (`wait_with_progress_*`) moved to
// `warden_sandbox::local`'s own test module -- that per-line callback is
// now dead code on this side (every remaining `wait` caller passes no
// callback at all; only `warden_sandbox::LocalSandbox::execute` still
// offers one, to the sandbox seam's own caller). See
// `warden_sandbox::local::tests::on_stdout_line_skips_blank_lines` and
// its neighbours for that coverage, unchanged in substance.

/// H1 (issue #20 review): an agent that exits immediately without ever
/// reading stdin at all must not fail the invocation — a broken pipe is
/// a legitimate outcome (logged, not silently swallowed), not a reason
/// to fail the run. The payload is deliberately larger than a typical
/// OS pipe buffer (64KiB) so the write is guaranteed to still be in
/// progress when the child exits and closes its read end, forcing a
/// genuine `ErrorKind::BrokenPipe` rather than racing a write that might
/// complete before the child even schedules to exit.
#[tokio::test]
async fn an_agent_that_never_reads_stdin_and_exits_immediately_does_not_fail_the_invocation() {
    let dir = TempDir::new().unwrap();
    let cmd = AgentCommand::new("sh", ["-c", "exit 0"]);
    let child = spawn(&cmd, dir.path()).unwrap();
    let large_payload = "x".repeat(200_000);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        wait(child, "sh", Some(large_payload), CancellationToken::new()),
    )
    .await
    .expect("wait must not hang on a broken pipe");

    let outcome = result
        .expect("a broken pipe from an agent that ignores stdin must not fail the invocation");
    assert_eq!(outcome.exit_code, 0);
}

/// H1 unit coverage for [`classify_stdin_write_error`]'s two branches.
/// The fatal (non-`BrokenPipe`) branch is exercised here rather than
/// through a real subprocess: deterministically forcing a write error
/// other than a broken pipe out of a genuine OS pipe isn't practical
/// (`EPIPE` is by far the dominant real-world case, already covered
/// end-to-end by `an_agent_that_never_reads_stdin_and_exits_immediately_does_not_fail_the_invocation`
/// above), so this isolates the classification decision itself.
#[test]
fn classify_stdin_write_error_treats_broken_pipe_as_non_fatal_and_anything_else_as_fatal() {
    let broken_pipe = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
    assert!(classify_stdin_write_error(broken_pipe, "agent").is_ok());

    let other = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    let result = classify_stdin_write_error(other, "agent");
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

// -----------------------------------------------------------------
// Re-test cycle (issue #20 review fix, fdcaa4e): adversarial stdin
// write-failure angles beyond the coder's own "never reads at all"
// case, derived from the task's intent independent of the coder's
// tests above.
// -----------------------------------------------------------------

/// Adversarial angle: an agent that reads only *part* of a large
/// payload before exiting (not "never reads at all") must still be a
/// non-fatal, logged outcome -- the broken pipe fires once the agent's
/// read end closes regardless of how much it already consumed.
#[tokio::test]
async fn an_agent_that_reads_only_part_of_the_payload_then_exits_does_not_fail_the_invocation() {
    let dir = TempDir::new().unwrap();
    let cmd = AgentCommand::new("sh", ["-c", "head -c 100 > /dev/null; exit 0"]);
    let child = spawn(&cmd, dir.path()).unwrap();
    let large_payload = "x".repeat(200_000);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        wait(child, "sh", Some(large_payload), CancellationToken::new()),
    )
    .await
    .expect("wait must not hang when the agent only partially reads stdin");

    let outcome = result
        .expect("an agent reading only part of the payload before exiting must not fail the run");
    assert_eq!(outcome.exit_code, 0);
}

/// Adversarial angle: an agent that explicitly closes its stdin file
/// descriptor mid-run (rather than exiting outright) must still see the
/// write fail as a non-fatal broken pipe -- and `wait` must not hang
/// waiting for the write to somehow complete once the read side is
/// gone, even though the process itself keeps running for a while
/// afterwards.
#[tokio::test]
async fn an_agent_that_closes_stdin_mid_write_while_continuing_to_run_does_not_fail_the_invocation()
{
    let dir = TempDir::new().unwrap();
    let cmd = AgentCommand::new("sh", ["-c", "exec 0<&-; sleep 0.3; exit 0"]);
    let child = spawn(&cmd, dir.path()).unwrap();
    let large_payload = "x".repeat(200_000);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        wait(child, "sh", Some(large_payload), CancellationToken::new()),
    )
    .await
    .expect("wait must not hang when the agent closes stdin mid-write and keeps running");

    let outcome = result.expect(
        "an agent that closes stdin mid-write but keeps running must not fail the invocation",
    );
    assert_eq!(outcome.exit_code, 0);
}

#[test]
fn current_process_is_reported_alive() {
    let pid = std::process::id();
    let start_time = process_start_time(pid).expect("start time available for the current process");
    assert!(is_process_alive(pid, start_time));
}

#[test]
fn a_pid_that_almost_certainly_does_not_exist_is_reported_not_alive() {
    // Real PIDs are far smaller than this on both Linux (< 2^22 by
    // default) and macOS (< 100_000); used purely as a deterministic
    // "not alive" fixture, well within the valid positive pid_t range.
    assert!(!is_process_alive(999_999_999, UNKNOWN_START_TIME));
}

#[test]
fn a_wrong_start_time_is_reported_not_alive_even_though_the_pid_exists() {
    // The core PID-reuse defence (H1): a PID that genuinely exists
    // right now must still be reported not-alive if the start time we
    // recorded for it doesn't match the process currently holding that
    // PID — that mismatch is exactly what happens when the original
    // process died and the OS handed its PID to something else later.
    let pid = std::process::id();
    let real_start_time = process_start_time(pid).unwrap();
    let bogus_start_time = real_start_time + 1_000_000;
    assert!(!is_process_alive(pid, bogus_start_time));
}

#[test]
fn no_recorded_start_time_falls_back_to_plain_existence_check() {
    // Historical/degraded case: UNKNOWN_START_TIME means we never
    // captured a fingerprint for this row, so we can't rule out PID
    // reuse — but we also shouldn't refuse to ever recover such rows,
    // so we fall back to "does a process with this PID exist at all".
    let pid = std::process::id();
    assert!(is_process_alive(pid, UNKNOWN_START_TIME));
}

/// Regression test for H1: POSIX `kill(pid=0, ...)` signals every
/// process in the caller's own process group, so a naive liveness check
/// against a pid-0 sentinel always misreported "alive" regardless of
/// whether pid 0 referred to a real agent — silently defeating the
/// crash-detection acceptance criterion in issue #1. `pid == 0` is now
/// an explicit sentinel that is never alive, and
/// `orchestrator::run_agent` no longer persists pid 0 at all (a missing
/// `Child::id()` is a typed `ProcessError::MissingPid`, not a silent
/// fallback to 0).
#[test]
fn pid_zero_is_never_reported_alive() {
    assert!(!is_process_alive(0, UNKNOWN_START_TIME));
    assert!(!is_process_alive(0, 12345));
}

#[tokio::test]
async fn kill_pid_terminates_a_live_process_with_a_matching_fingerprint() {
    let dir = TempDir::new().unwrap();
    let cmd = AgentCommand::new("sh", ["-c", "sleep 30"]);
    let mut child = spawn(&cmd, dir.path()).unwrap();
    let pid = child.id().unwrap();
    let start_time = process_start_time(pid).unwrap();

    kill_pid(pid, start_time).unwrap();

    // `wait()` blocks until the OS has reaped it — proves the signal
    // actually landed, not just that `kill_pid` returned `Ok`.
    let status = child.wait().await.unwrap();
    assert!(!status.success());
    assert!(!is_process_alive(pid, start_time));
}

#[tokio::test]
async fn kill_pid_is_a_noop_when_the_fingerprint_no_longer_matches() {
    // H1 regression: a live process that genuinely exists at `pid` must
    // never be signalled if its recorded start time doesn't match —
    // that mismatch is exactly the PID-reuse case this guards against.
    let dir = TempDir::new().unwrap();
    let cmd = AgentCommand::new("sh", ["-c", "sleep 30"]);
    let mut child = spawn(&cmd, dir.path()).unwrap();
    let pid = child.id().unwrap();
    let real_start_time = process_start_time(pid).unwrap();
    let bogus_start_time = real_start_time + 1_000_000;

    kill_pid(pid, bogus_start_time).unwrap();

    // Still alive: the mismatched fingerprint must have stopped
    // `kill_pid` from touching it.
    assert!(is_process_alive(pid, real_start_time));
    child.kill().await.unwrap();
}

#[test]
fn kill_pid_on_pid_zero_is_a_noop_not_a_signal_to_the_process_group() {
    assert!(kill_pid(0, UNKNOWN_START_TIME).is_ok());
}

#[test]
fn kill_pid_on_an_already_dead_pid_is_a_noop() {
    assert!(kill_pid(999_999_999, UNKNOWN_START_TIME).is_ok());
}

/// Issue #32: `spawn_tui_attach` must invoke `<binary> attach --run-id
/// <id> --warden-home <path>` verbatim. Captures argv to a file instead
/// of stdout, since [`spawn_tui_attach`]'s whole point is inheriting
/// stdio (the real `warden-tui` must take over the launch terminal), not
/// piping it for a test to capture.
#[cfg(unix)]
#[tokio::test]
async fn spawn_tui_attach_passes_the_expected_attach_subcommand_and_flags() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let out_file = dir.path().join("captured-args.txt");
    let script_path = dir.path().join("fake-warden-tui");
    std::fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
            out_file.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();

    let warden_home = dir.path().join("home");
    let mut child = spawn_tui_attach(&script_path, "run-123", &warden_home).unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success());

    let captured = std::fs::read_to_string(&out_file).unwrap();
    assert_eq!(
        captured.lines().collect::<Vec<_>>(),
        vec![
            "attach",
            "--run-id",
            "run-123",
            "--warden-home",
            warden_home.to_str().unwrap(),
        ]
    );
}

/// Unlike [`spawn`] (which `env_clear()`s for agent isolation),
/// `spawn_tui_attach` must inherit the full parent environment --
/// `warden-tui` is a trusted first-party binary, not an agent under the
/// Agent Subprocess Protocol. Checked against `PATH`, whatever it
/// already is in the test process, rather than mutating global process
/// environment state (which `std::env::set_var` would, unsafely and with
/// cross-test interference risk under a parallel test runner).
#[cfg(unix)]
#[tokio::test]
async fn spawn_tui_attach_inherits_the_full_parent_environment() {
    use std::os::unix::fs::PermissionsExt;

    let expected_path = std::env::var("PATH").expect("PATH is set in the test process");

    let dir = TempDir::new().unwrap();
    let out_file = dir.path().join("captured-env.txt");
    let script_path = dir.path().join("fake-warden-tui");
    std::fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nprintf '%s' \"$PATH\" > \"{}\"\n",
            out_file.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();

    let mut child = spawn_tui_attach(&script_path, "run-123", &dir.path().join("home")).unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success());

    assert_eq!(std::fs::read_to_string(&out_file).unwrap(), expected_path);
}

#[tokio::test]
async fn spawn_tui_attach_reports_a_typed_error_when_the_binary_does_not_exist() {
    let dir = TempDir::new().unwrap();
    let missing_binary = dir.path().join("does-not-exist");
    let result = spawn_tui_attach(&missing_binary, "run-123", &dir.path().join("home"));
    assert!(matches!(result, Err(ProcessError::Spawn { .. })));
}
