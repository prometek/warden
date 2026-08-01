//! Shared convention for host-side `git` invocations that touch the base repository
//! (`RunConfig::repo_path`) or any worktree checked out under `<warden_home>/worktrees/...`.
pub(crate) const NO_HOST_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];
