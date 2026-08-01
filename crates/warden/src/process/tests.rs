use super::*;
use std::path::PathBuf;
use tempfile::TempDir;

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

#[test]
fn an_invented_scheme_in_front_of_a_relative_path_does_not_bypass_the_guard() {
    let layout = WorktreeLayout::new();
    let reviewer_worktree = layout.role_worktree("reviewer");
    let coder_worktree = layout.role_worktree("coder");
    let repo = TempDir::new().unwrap();
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

#[tokio::test]
async fn writing_a_large_stdin_payload_does_not_deadlock_on_large_stdout() {
    let dir = TempDir::new().unwrap();
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
    assert!(!is_process_alive(999_999_999, UNKNOWN_START_TIME));
}

#[test]
fn a_wrong_start_time_is_reported_not_alive_even_though_the_pid_exists() {
    let pid = std::process::id();
    let real_start_time = process_start_time(pid).unwrap();
    let bogus_start_time = real_start_time + 1_000_000;
    assert!(!is_process_alive(pid, bogus_start_time));
}

#[test]
fn no_recorded_start_time_falls_back_to_plain_existence_check() {
    let pid = std::process::id();
    assert!(is_process_alive(pid, UNKNOWN_START_TIME));
}

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

    let status = child.wait().await.unwrap();
    assert!(!status.success());
    assert!(!is_process_alive(pid, start_time));
}

#[tokio::test]
async fn kill_pid_is_a_noop_when_the_fingerprint_no_longer_matches() {
    let dir = TempDir::new().unwrap();
    let cmd = AgentCommand::new("sh", ["-c", "sleep 30"]);
    let mut child = spawn(&cmd, dir.path()).unwrap();
    let pid = child.id().unwrap();
    let real_start_time = process_start_time(pid).unwrap();
    let bogus_start_time = real_start_time + 1_000_000;

    kill_pid(pid, bogus_start_time).unwrap();

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
