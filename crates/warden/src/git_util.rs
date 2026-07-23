//! Shared convention for host-side `git` invocations that touch the base
//! repository (`RunConfig::repo_path`) or any worktree checked out under
//! `<warden_home>/worktrees/...` (issue #49 review, HIGH).
//!
//! Hooks live in the *common* git dir, shared by the main repo and every one
//! of its worktrees (`git worktree add` doesn't get its own `hooks/`). Under
//! `--isolation docker`, `warden_sandbox::DockerSandbox` bind-mounts the base
//! repo's `.git` **read-write** into the container (see that module's own
//! docs for why: a worktree's `gitdir:` pointer must resolve there). A
//! contained agent can therefore write `.git/hooks/pre-push`,
//! `post-checkout`, `reference-transaction`, `pre-commit`, etc. -- and have
//! it run **on the host**, as the host user, the next time `warden` itself
//! (not the agent) runs a git command against that same repo or one of its
//! worktrees. `warden` does exactly that: `push_converged_commit_to_bare_repo`
//! runs `git push` against `repo_path` once a run converges, `protect_cycle_commit`/
//! `evidence::commit_evidence_into_repo` run `update-ref`/`commit` against it,
//! `WorktreeManager::create` runs `worktree add` (a checkout). A planted hook
//! would run with the host's full ambient access (`~/.ssh`, `~/.aws`, real
//! push credentials) -- defeating the exact #25/#28 guarantee `--isolation
//! docker` exists to provide, entirely on the host side, no matter how tight
//! the container's own mounts are.
//!
//! [`NO_HOST_HOOKS`] neutralizes this: `-c core.hooksPath=<value>` on the
//! command line always overrides whatever `core.hooksPath` the repo's own
//! (agent-writable) `.git/config` sets, so an agent editing that config
//! cannot re-enable hooks behind this flag. `/dev/null` is deliberate, not a
//! typo -- verified directly against real git (issue #49 review) that a
//! `core.hooksPath` which is not a directory is treated exactly like "hook
//! script not found": git silently skips the hook rather than erroring, for
//! every hook exercised (`pre-commit`, `pre-push`, `reference-transaction`,
//! `post-checkout`).
//!
//! Every host-side git invocation in this crate that touches `repo_path` or
//! a worktree under it passes this. A git invocation against a path this
//! crate owns exclusively, that an agent never has any (even indirect, via a
//! shared object store) write access to -- e.g. `warden-gated`'s own local
//! bare gate repo, never mounted into any sandbox -- does not need it: there
//! is no hook an agent could have planted there in the first place.
pub(crate) const NO_HOST_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];
