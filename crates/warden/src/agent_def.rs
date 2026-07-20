//! Resolves a role's markdown agent definition (issue #24) -- by
//! **convention**, `<repo>/.warden/agents/{coder,reviewer,tester}.md`, or the
//! selected `--tool` adapter's own default prompt when no such file exists.
//! This is what makes `warden run --repo <path> --intent "..." --tool
//! claude` work with zero markdown at all: unlike the removed `--coder-
//! agent`/`--reviewer-agent`/`--tester-agent` flags (ADR-0013, issue #22),
//! there is no required path for the user to supply.
//!
//! I/O only: the schema, its validation, and every rule about what a
//! definition may say live in `warden_core::agent_def` (pure, testable
//! without a filesystem), mirroring the `warden_core::agent_wire` /
//! `warden::process` split. This module's job is "does the convention file
//! exist -- if so, read and validate it, naming the file on either failure;
//! if not, ask the adapter for its default".
//!
//! # Security: resolved once, from the run's base repo, before any agent runs
//!
//! [`resolve_agent_definition`] is called exactly once per role, against
//! `RunConfig::repo_path` (the run's **base** repository -- the path the
//! user passed to `--repo`), at the very start of
//! `Orchestrator::run_convergence_loop`, before the coder (or anyone else)
//! is spawned. The three resulting [`warden_core::AgentDefinition`]s are
//! then held in memory (`RunConfig`) for the run's entire lifetime -- they
//! are **never** re-read from a role's own worktree, or re-resolved
//! mid-run, no matter how many cycles a run goes through.
//!
//! This is deliberate, not an oversight: the coder is the one role that
//! writes to the repository, on a branch that becomes the reviewer's and
//! tester's own worktree once the coder commits (`WorktreeManager::create`
//! syncs each role's worktree onto the coder's latest commit every cycle,
//! see `orchestrator.rs`). If reviewer/tester definitions were re-read from
//! that same moving branch per invocation instead of snapshotted once at run
//! start, a coder -- buggy, or an agent that has gone off the rails under
//! prompt injection from the very code it's reviewing -- could commit a
//! rewritten `.warden/agents/reviewer.md` (a different system prompt, a
//! looser `model`, a `tools` list stripping away whatever safety
//! constraints the prompt relied on) and have its own reviewer or tester
//! evaluate it under rules the coder itself just chose. That would defeat
//! the entire point of running reviewer/tester as an independent gate
//! (Architecture.md §1, "Reviewer: ... remonte des findings de sécurité/
//! implémentation" -- independent of the coder's own judgement). Resolving
//! from the base repo, once, before the coder ever runs, keeps the review
//! gate's own configuration outside the coder's reach for the run's whole
//! duration, exactly like `warden-gated` never trusts a notification alone
//! without revalidating against its own read-only state (ADR-0006).
//!
//! This reading of the ticket's otherwise-unwritten security requirement
//! ("dépend des décisions de sécurité ci-dessous (isolation filesystem,
//! définitions coder-controllables)", issue #24) was confirmed with the
//! issue's owner (issue #24 review, M4). The snapshot documented above
//! closes the *within-run* half of it: a coder cannot rewrite the very
//! reviewer/tester that will judge its own current run, because that
//! judgement was already resolved before the coder ever ran. It does
//! **not** close the *cross-run* half by itself: nothing stops a coder from
//! committing a change under `.warden/agents/` that this run's reviewer/
//! tester happily approves (it's just a file to them, same as any other),
//! and once that commit merges, it becomes the convention file the *next*
//! `warden run` against this repo resolves -- unreviewed by anything but
//! the very cycle whose coder wrote it.
//!
//! The owner's ruling on that second half: `.warden/agents/` stays writable
//! and committable by the coder -- banning writes there outright was
//! rejected, since it would break the legitimate "improve our own agent
//! prompts" workflow issue #24 exists to enable in the first place, and
//! deferring the whole question to a follow-up ticket was also rejected, as
//! cheap enough to close in this same pass. Instead, detection: a cycle
//! whose coder diff touches anything under `.warden/agents/` (add, modify,
//! delete, or rename in either direction) raises a **blocking**
//! `FindingSource::Warden` finding (`orchestrator::agent_definition_tampering_finding`),
//! through the exact same findings/severity machinery a reviewer/tester/CI
//! finding already goes through -- so the change can never merge, and thus
//! can never reach a future run, without a human noticing and reviewing it
//! first. Together, the two halves are: within-run isolation (this
//! module's snapshot) + cross-run detection (the blocking finding). Neither
//! is a credential/filesystem sandbox around the coder itself -- the coder
//! still runs with real repo access and whatever the selected `--tool`
//! adapter's default grants allow (`ClaudeAdapter::default_tools`, `Bash`
//! included); that broader exposure is an accepted, owner-reviewed trade
//! for now, tracked for real isolation in issue #28.
//!
//! # Owner's ruling: the same coder-writable-directory concern also covers a stale worktree (issue #26 review, escalated asymmetry)
//!
//! [`crate::process::validate_agent_program`]'s `program` containment check
//! refuses a path resolving inside `worktree_path`, `repo_path`, *or*
//! `run_worktrees_root` (`<warden_home>/worktrees/<run_id>/`) -- the coder's
//! own worktree for this run is exactly as coder-writable as the base repo
//! itself (`Bash` included), so both are refused symmetrically. The HIGH fix
//! below (see "the trusted user-config directory is verified, not assumed")
//! originally compared `user_config_agents_dir`/`<role>.md` against
//! `repo_path` only, leaving an asymmetry a reviewer flagged: an
//! `XDG_CONFIG_HOME` resolving under `<warden_home>/worktrees/` -- e.g. a
//! stale worktree left behind by a crashed run, referenced by a committed
//! `.envrc` a human's shell still picks up on the *next* invocation -- would
//! resolve as trusted [`AgentDefinitionSource::UserConfig`], even though
//! that directory was written to by a previous run's coder exactly as freely
//! as `repo_path` itself.
//!
//! The owner's ruling: close the asymmetry. [`user_config_resolves_inside_repo_or_worktrees`]
//! degrades a user-config dir or `<role>.md` resolving under
//! `<warden_home>/worktrees/` (the whole worktrees tree this `warden_home`
//! has ever created worktrees under, not just this not-yet-created run's own
//! `<run_id>` subdirectory -- at the point this check runs, this run's own
//! `run_id` doesn't exist yet, so only a *stale* worktree from an earlier run
//! is even reachable here) to [`AgentDefinitionSource::UntrustedRepoOverride`]
//! exactly like one resolving inside `repo_path` -- same degrade-not-fail
//! rationale, same `--trust-repo-agents` gate, same warning.
//!
//! # Security: role-asymmetric resolution (issue #26)
//!
//! The within-run snapshot above closes half of issue #26's concern (a
//! coder can't rewrite the reviewer/tester judging its own current run) and
//! the tampering finding closes the other cross-run half (a rewrite can't
//! *silently* reach a future run) -- but neither one changes *where* a
//! future run's reviewer/tester definition is read from in the first place:
//! before this issue, it was always `<repo>/.warden/agents/<role>.md`, a
//! path inside the very repository the coder commits to. A human reviewing
//! (and approving) a `.warden/agents/reviewer.md` change is the *only*
//! thing standing between a coder-authored prompt and a future run's
//! reviewer -- fine as defence in depth, not fine as the *sole* mechanism
//! for a role whose entire purpose is judging the coder independently.
//!
//! The owner's ruling (confirmed, not re-litigated here): resolution is
//! **role-asymmetric**.
//!
//! - **Coder**: unchanged from issue #24 -- `<repo>/.warden/agents/coder.md`
//!   if present, else the adapter's default. The coder is already the
//!   repo's own untrusted role (it writes the repo; nothing is gained by
//!   hiding its own prompt from itself), and locking this path away would
//!   break the zero-config "no markdown at all" UX issue #24 exists to
//!   provide, for no security benefit.
//! - **Reviewer/Tester**: resolved from the **user config directory only**
//!   -- `$XDG_CONFIG_HOME/warden/agents/<role>.md`, falling back to
//!   `~/.config/warden/agents/<role>.md` (see
//!   [`default_user_config_agents_dir`]) -- which the coder has no write
//!   access to at all; it lives outside every worktree `WorktreeManager`
//!   ever creates and outside `RunConfig::repo_path` itself. The repo's own
//!   `.warden/agents/{reviewer,tester}.md` is **ignored by default**, even
//!   though the very same file is still honoured for the coder.
//! - **`--trust-repo-agents` (opt-in, default off)**: when set, a reviewer/
//!   tester definition absent from the user config directory falls back to
//!   the repo's own `.warden/agents/<role>.md` exactly as it did before this
//!   issue -- but only ever as a fallback, and only ever surfaced as
//!   untrusted: [`resolve_agent_definition`] logs a `tracing::warn!` naming
//!   the exact path the moment it reads one, and the caller
//!   (`orchestrator::Orchestrator::run_convergence_loop`) publishes a
//!   [`warden_core::RunEvent::UntrustedAgentDefinitionUsed`] for it right
//!   after `RunStarted`, so the run's own permanent, replayable event log
//!   (not just this process's own stderr) carries a record of which role(s)
//!   ran under a definition the coder can write to. This flag exists for
//!   the legitimate case where a project genuinely wants to version its own
//!   reviewer/tester prompts alongside its code and accepts the trade-off
//!   -- it must never be silently indistinguishable from a trusted
//!   resolution.
//!
//! **Precedence when both exist** (`--trust-repo-agents` set, and *both* a
//! user-config file and a repo file exist for the same role): the user
//! config file **always wins**. The repo file is consulted at all only when
//! no user-config file exists for that role -- an override for the *common*
//! case (nothing in `~/.config/warden/agents/` yet), never a way for a
//! project's own repo to shadow a value the user deliberately configured for
//! themselves system-wide.
//!
//! A file that exists but fails to read or parse is a typed error naming
//! the path -- the exact same rule issue #24 already established for the
//! coder's convention file, extended here to the user-config path and the
//! opt-in repo fallback alike (see [`resolve_agent_definition`]'s own
//! docs). Only a **missing** file at a given location falls through to the
//! next one in precedence order; nothing is ever silently treated as
//! "absent" because it failed to read.
//!
//! # Security: the "trusted" user-config directory is verified, not assumed (issue #26 review, HIGH)
//!
//! The role-asymmetric split above is only as strong as the assumption that
//! [`default_user_config_agents_dir`]'s result is genuinely outside the
//! coder's reach. Left unverified, it is not: a coder can commit a
//! `.envrc`/nix `devShell`/`Makefile` wrapper that exports
//! `XDG_CONFIG_HOME=$PWD/.config` (direnv, or any dev-environment tooling a
//! human's shell picks up before running `warden run --repo .`), or plant a
//! symlink `~/.config/warden/agents/reviewer.md -> <repo>/.warden/agents/reviewer.md`.
//! Either way, `<user_config_agents_dir>/reviewer.md` would then *be* a
//! coder-written, coder-committed repo file, yet resolve as
//! [`AgentDefinitionSource::UserConfig`] -- no warning, no
//! `RunEvent::UntrustedAgentDefinitionUsed`, and `--trust-repo-agents` not
//! even required. The reviewer judging the coder would be a prompt the coder
//! itself wrote, with the audit trail asserting the opposite. This is
//! exactly the asymmetry [`crate::process::validate_agent_program`]'s own
//! containment check already closes for `command.program` -- the reviewer/
//! tester's `program` is never trusted just because an adapter says so, it
//! is canonicalized and checked against the repo/worktree roots first. The
//! same discipline belongs here.
//!
//! So [`resolve_agent_definition`] canonicalizes both
//! `user_config_agents_dir` and the resolved `<user_config_agents_dir>/<role>.md`
//! (resolving any symlink in either, exactly like the `command.program`
//! check) and compares them against the canonicalized `repo_path` --
//! and, per the owner's ruling above ("the same coder-writable-directory
//! concern also covers a stale worktree"), against `<warden_home>/worktrees/`
//! too. When *any* of those resolve inside the repo under review or a
//! worktree, this is **not** a hard failure -- a hard failure here would be a
//! denial-of-service any coder could trigger against its own reviewer/tester
//! just by planting a throwaway `XDG_CONFIG_HOME` override, and it would
//! erase the distinction between "genuinely misconfigured" and "actively
//! adversarial" that this module otherwise preserves. Instead, the
//! resolution is **degraded**: treated exactly like a repo-sourced
//! definition (the same [`AgentDefinitionSource::UntrustedRepoOverride`]
//! variant, subject to the exact same `--trust-repo-agents` gate, the same
//! `tracing::warn!` naming the path the moment it is actually read, and the
//! same "ignored but warned about" treatment when the flag is off -- see
//! [`resolve_agent_definition`]'s own docs). The trust label a caller
//! observes always matches what the path actually is, never what
//! `XDG_CONFIG_HOME`/`HOME` merely claim it is.

use std::path::{Path, PathBuf};

use warden_core::{parse_agent_definition, AgentDefinition, AgentRole};

use crate::error::{AgentDefinitionError, Result};
use crate::path_util::canonicalize_best_effort;
use crate::tool_adapter::ToolAdapter;

/// The directory, relative to a repo's root, that holds convention-based
/// agent definitions (issue #24 point 3): `.warden/agents/coder.md`,
/// `.warden/agents/reviewer.md`, `.warden/agents/tester.md`.
///
/// `pub(crate)`: also read by `orchestrator::agent_definition_tampering_finding`
/// (issue #24 review, M4) to recognize a coder diff touching this same
/// convention directory -- kept as this module's one definition of the
/// convention path rather than duplicated as a second string literal
/// elsewhere in the crate, so the two can never silently drift apart.
pub(crate) const AGENTS_DIR: &str = ".warden/agents";

/// Where a resolved [`AgentDefinition`] actually came from (issue #26) --
/// lets a caller (`main.rs`) tell a reviewer/tester definition sourced from
/// the trusted user config directory apart from one sourced from the repo
/// under review, the one case that must be surfaced as untrusted (see this
/// module's own "Security: role-asymmetric resolution" docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDefinitionSource {
    /// Coder only: `<repo>/.warden/agents/coder.md`. Not "untrusted" in the
    /// sense issue #26 cares about -- the coder is already the repo's own
    /// untrusted role, so nothing is gained by treating its own prompt file
    /// specially.
    RepoConvention(PathBuf),
    /// Reviewer/tester only: `<user_config_agents_dir>/<role>.md`. The
    /// trusted source -- outside the coder's write access entirely.
    UserConfig(PathBuf),
    /// Reviewer/tester only, and only reachable with `trust_repo_agents:
    /// true`: either `<repo>/.warden/agents/<role>.md` (no user-config file
    /// for that role), or -- issue #26 review, HIGH / escalated asymmetry --
    /// a would-be `<user_config_agents_dir>/<role>.md` that canonicalizes
    /// inside `repo_path` or `<warden_home>/worktrees/` (a coder-controlled
    /// `XDG_CONFIG_HOME`/symlink, or a stale worktree from a crashed run;
    /// see this module's own "the trusted user-config directory is
    /// verified, not assumed" docs), degraded to this same untrusted
    /// treatment instead of being silently accepted as
    /// [`AgentDefinitionSource::UserConfig`]. The one variant a caller must
    /// surface to the run as untrusted.
    UntrustedRepoOverride {
        /// The literal, pre-canonicalization path that was actually read --
        /// what an operator recognizes: `<repo>/.warden/agents/<role>.md`
        /// for the plain repo-convention case, or the would-be
        /// `<user_config_agents_dir>/<role>.md` for the degraded case.
        path: PathBuf,
        /// Issue #26 review, LOW: what `path` actually canonicalizes to
        /// (symlinks resolved). For the plain repo-convention case this
        /// usually agrees with `path` itself; for the degraded case (a
        /// coder-controlled `XDG_CONFIG_HOME`, or a symlinked `<role>.md`)
        /// `path` may not *look* like it is inside the repo/a worktree at
        /// all -- e.g. `~/.config/warden/agents/reviewer.md` -- while this
        /// field names exactly where it actually resolved to. Carried
        /// alongside `path`, never instead of it: `path` is what the
        /// operator typed/configured and recognizes, `canonical_path` is
        /// what it was actually proven to be.
        canonical_path: PathBuf,
    },
    /// No file at any location this role consults -- the selected tool
    /// adapter's own default prompt/tools.
    AdapterDefault,
}

/// Resolves `role`'s definition for this run (issue #24, extended by issue
/// #26's role-asymmetric trust -- see this module's own docs for the full
/// rationale). See the module docs for why this must only ever be called
/// once per role, at run start, against the base repo.
///
/// - **Coder**: `<repo>/.warden/agents/coder.md` if present, else
///   `adapter`'s own default -- unchanged from issue #24.
/// - **Reviewer/Tester**: `<user_config_agents_dir>/<role>.md` if present
///   *and* it genuinely resolves outside both `repo_path` and
///   `<warden_home>/worktrees/` (issue #26 review, HIGH / escalated
///   asymmetry -- see this module's own "the trusted user-config directory
///   is verified, not assumed" and "the same coder-writable-directory
///   concern also covers a stale worktree" docs); else, only when
///   `trust_repo_agents` is `true`, either that same user-config path (if it
///   *does* resolve inside one of those) or `<repo>/.warden/agents/<role>.md`,
///   whichever is checked first and actually has a file (logged with
///   `tracing::warn!`, naming the path, the moment it's actually read -- see
///   [`AgentDefinitionSource::UntrustedRepoOverride`]); else `adapter`'s own
///   default. `user_config_agents_dir` and `warden_home` are explicit
///   parameters rather than resolved/derived from the environment in here --
///   see [`default_user_config_agents_dir`]'s own docs on why, and how a
///   real caller (`main.rs`) obtains them.
///
/// Returns the resolved definition alongside where it came from
/// ([`AgentDefinitionSource`]) -- callers that don't care (most of them) can
/// simply discard the second element; `main.rs` uses it to decide which
/// reviewer/tester resolutions need surfacing as untrusted.
///
/// A convention/config file that exists but fails to read (permissions, not
/// a regular file, ...) or fails to parse (bad frontmatter, unknown key,
/// blank prompt, ...) is a typed error naming the path -- never silently
/// treated as "absent, fall back to the next source". Only a **missing**
/// file (`io::ErrorKind::NotFound`) falls through; any other read failure is
/// an [`AgentDefinitionError::Read`] naming the path, exactly like a parse
/// failure is an [`AgentDefinitionError::Invalid`] naming it. This applies
/// identically at every location consulted: the coder's convention file,
/// the reviewer/tester's user-config file, and the reviewer/tester's opt-in
/// repo fallback all go through the same [`try_read_definition`].
///
/// # An ignored repo-controlled file is warned about, never silently dropped (issue #26 review, MEDIUM)
///
/// A reviewer/tester file that exists at a repo-controlled location --
/// `<repo>/.warden/agents/<role>.md`, or a user-config path degraded per the
/// HIGH fix above -- but is skipped because `trust_repo_agents` is `false`
/// still gets a `tracing::warn!` naming the path, via
/// [`try_untrusted_repo_source`]'s own `tokio::fs::try_exists` check (an
/// **existence check only** -- the file itself is never opened, preserving
/// the property that an untrusted file is never read at all unless the flag
/// is set). Without this, a user following an older README's repo-sourced
/// convention would be silently switched onto the adapter's default prompt
/// with no indication anything was ignored (code-standards.md: "no silent
/// fallback").
///
/// # A *present* file that omits `tools` still gets the adapter's default
/// grant (issue #24 review finding B2)
///
/// "Every frontmatter key is optional" (issue #24 point 3, pinned by
/// `warden_core::agent_def::tests::every_frontmatter_key_is_optional`) is a
/// deliberately supported case: a definition file that only wants to
/// override the system prompt, leaving `tools`/`model` to the adapter, must
/// work. But `warden_core::AgentDefinition::tools` being `None` is not a
/// neutral "no opinion" for every adapter -- verified directly against the
/// real CLI (`ClaudeAdapter`'s own docs), a `claude -p` invocation with no
/// `--allowedTools` at all denies every tool call outright. If a *present*
/// file's omitted `tools` reached `ToolAdapter::build_command` as `None`
/// unchanged, the agent would be silently muzzled: a reviewer that can't
/// call any tool raises zero findings, a coder that can't `Write`/`Bash`
/// commits nothing, and `decide_next_state` sees a clean cycle and
/// converges -- a false pass, not a real one. So [`try_read_definition`]
/// only ever calls `parse_agent_definition` for a *present* file, then
/// merges `adapter.default_tools(role)` in wherever the parsed `tools` came
/// back `None` -- exactly as if the file had spelled the default out
/// itself. A file that *does* set `tools` (even to something the adapter
/// wouldn't have chosen) is never touched: only an omitted key is filled
/// in, never an explicit one overridden.
pub async fn resolve_agent_definition(
    repo_path: &Path,
    role: AgentRole,
    adapter: &impl ToolAdapter,
    user_config_agents_dir: &Path,
    warden_home: &Path,
    trust_repo_agents: bool,
) -> Result<(AgentDefinition, AgentDefinitionSource)> {
    match role {
        AgentRole::Coder => {
            let path = repo_path
                .join(AGENTS_DIR)
                .join(format!("{}.md", role.as_str()));
            match try_read_definition(&path, role, adapter).await? {
                Some(definition) => Ok((definition, AgentDefinitionSource::RepoConvention(path))),
                None => Ok((
                    adapter_default_definition(role, adapter)?,
                    AgentDefinitionSource::AdapterDefault,
                )),
            }
        }
        AgentRole::Reviewer | AgentRole::Tester => {
            let user_config_path = user_config_agents_dir.join(format!("{}.md", role.as_str()));
            // Owner's ruling ("the same coder-writable-directory concern
            // also covers a stale worktree"): the whole worktrees tree this
            // `warden_home` has ever created worktrees under, not just this
            // not-yet-created run's own `<run_id>` subdirectory -- see this
            // module's own docs.
            let warden_home_worktrees_root = warden_home.join("worktrees");

            // Issue #26 review, HIGH / escalated asymmetry: a "trusted"
            // user-config location that actually resolves inside the repo
            // under review or this warden home's own worktrees (a
            // coder-controlled `XDG_CONFIG_HOME` override, a symlink, or a
            // stale worktree referenced by a committed `.envrc`) is never
            // treated as `UserConfig` -- see this module's own docs.
            match user_config_resolves_inside_repo_or_worktrees(
                user_config_agents_dir,
                &user_config_path,
                repo_path,
                &warden_home_worktrees_root,
            )? {
                None => {
                    if let Some(definition) =
                        try_read_definition(&user_config_path, role, adapter).await?
                    {
                        return Ok((
                            definition,
                            AgentDefinitionSource::UserConfig(user_config_path),
                        ));
                    }
                }
                Some(canonical_user_config_path) => {
                    if let Some(result) = try_untrusted_repo_source(
                        role,
                        &user_config_path,
                        adapter,
                        trust_repo_agents,
                        Some(&canonical_user_config_path),
                    )
                    .await?
                    {
                        return Ok(result);
                    }
                }
            }

            let repo_override_path = repo_path
                .join(AGENTS_DIR)
                .join(format!("{}.md", role.as_str()));
            if let Some(result) = try_untrusted_repo_source(
                role,
                &repo_override_path,
                adapter,
                trust_repo_agents,
                None,
            )
            .await?
            {
                return Ok(result);
            }

            Ok((
                adapter_default_definition(role, adapter)?,
                AgentDefinitionSource::AdapterDefault,
            ))
        }
    }
}

/// Whether `user_config_agents_dir` (or the specific `<role>.md` path
/// resolved under it) actually resolves inside `repo_path` or
/// `warden_home_worktrees_root` (`<warden_home>/worktrees/`) -- issue #26
/// review, HIGH, extended by the owner's ruling on the escalated asymmetry
/// ("the same coder-writable-directory concern also covers a stale
/// worktree"): all four paths are canonicalized (resolving any symlink)
/// before the comparison, exactly like
/// [`crate::process::validate_agent_program`]'s own `command.program`
/// containment check, so a coder-controlled `XDG_CONFIG_HOME` pointed at (or
/// symlinked into) the repo under review or a stale worktree can't slip past
/// a purely lexical comparison. See this module's own "the trusted
/// user-config directory is verified, not assumed" docs for why this
/// degrades the resolution rather than failing the run outright.
///
/// Returns `Ok(None)` when `user_config_path` is genuinely trusted, or
/// `Ok(Some(canonical_user_config_path))` (the canonicalized
/// `<user_config_agents_dir>/<role>.md`) when it is degraded -- the caller
/// threads that canonical path into [`try_untrusted_repo_source`] so both
/// the warning and [`AgentDefinitionSource::UntrustedRepoOverride`] can name
/// exactly where it actually resolved to (issue #26 review, LOW), without
/// canonicalizing the same path a second time.
///
/// Fails closed ([`AgentDefinitionError::PathResolutionFailed`]) if any of
/// the four paths can't be canonicalized for a reason other than "doesn't
/// exist yet" -- code-standards.md's "no silent fallback": a containment
/// check this function could no longer actually perform must never be
/// silently skipped.
fn user_config_resolves_inside_repo_or_worktrees(
    user_config_agents_dir: &Path,
    user_config_path: &Path,
    repo_path: &Path,
    warden_home_worktrees_root: &Path,
) -> Result<Option<PathBuf>> {
    let canonical_repo = canonicalize_best_effort(repo_path).map_err(|source| {
        AgentDefinitionError::PathResolutionFailed {
            path: repo_path.to_path_buf(),
            source,
        }
    })?;
    let canonical_worktrees_root =
        canonicalize_best_effort(warden_home_worktrees_root).map_err(|source| {
            AgentDefinitionError::PathResolutionFailed {
                path: warden_home_worktrees_root.to_path_buf(),
                source,
            }
        })?;
    let canonical_dir = canonicalize_best_effort(user_config_agents_dir).map_err(|source| {
        AgentDefinitionError::PathResolutionFailed {
            path: user_config_agents_dir.to_path_buf(),
            source,
        }
    })?;
    let canonical_path = canonicalize_best_effort(user_config_path).map_err(|source| {
        AgentDefinitionError::PathResolutionFailed {
            path: user_config_path.to_path_buf(),
            source,
        }
    })?;

    let degraded = canonical_dir.starts_with(&canonical_repo)
        || canonical_path.starts_with(&canonical_repo)
        || canonical_dir.starts_with(&canonical_worktrees_root)
        || canonical_path.starts_with(&canonical_worktrees_root);

    Ok(degraded.then_some(canonical_path))
}

/// Attempts to use `candidate_path` as a repo-controlled ("untrusted")
/// reviewer/tester definition source -- shared by both the repo's own
/// `.warden/agents/<role>.md` convention file and a would-be "trusted"
/// user-config resolution that turned out to canonicalize inside the repo or
/// a worktree (issue #26 review, HIGH). Both must be judged by the exact
/// same rule:
///
/// - `trust_repo_agents == false`: never opened. If a file genuinely exists
///   there (`tokio::fs::try_exists`, an existence check only), a
///   `tracing::warn!` names the path so it is never silently dropped (issue
///   #26 review, MEDIUM) -- either way, this returns `Ok(None)`, telling the
///   caller to try the next source in precedence order.
/// - `trust_repo_agents == true`: read via [`try_read_definition`]. `Some`
///   is a `tracing::warn!` naming the path plus
///   [`AgentDefinitionSource::UntrustedRepoOverride`]; `None` (no file
///   there) is `Ok(None)`, same as above.
///
/// `degraded_user_config_canonical_path` distinguishes the two situations
/// this fn is shared between (issue #26 review, LOW: the two need different
/// advice -- "move it to `$XDG_CONFIG_HOME/warden/agents/`" is a no-op for a
/// file that is *already* there, just resolving somewhere unexpected):
/// `None` means `candidate_path` is the plain `<repo>/.warden/agents/<role>.md`
/// convention file; `Some(canonical)` means `candidate_path` is a would-be
/// user-config path that [`user_config_resolves_inside_repo_or_worktrees`]
/// already proved resolves to `canonical`, inside the repo or a worktree --
/// reused here rather than canonicalized a second time.
async fn try_untrusted_repo_source(
    role: AgentRole,
    candidate_path: &Path,
    adapter: &impl ToolAdapter,
    trust_repo_agents: bool,
    degraded_user_config_canonical_path: Option<&Path>,
) -> Result<Option<(AgentDefinition, AgentDefinitionSource)>> {
    if !trust_repo_agents {
        let exists = tokio::fs::try_exists(candidate_path)
            .await
            .map_err(|source| AgentDefinitionError::Read {
                path: candidate_path.to_path_buf(),
                source,
            })?;
        if exists {
            match degraded_user_config_canonical_path {
                Some(canonical) => tracing::warn!(
                    role = role.as_str(),
                    path = %candidate_path.display(),
                    resolves_to = %canonical.display(),
                    "ignoring a reviewer/tester definition that looked like the trusted user \
                     config source but actually resolves inside the repo under review or this \
                     warden home's own worktrees (see `resolves_to`), both of which the coder \
                     can write to; point XDG_CONFIG_HOME/HOME at a directory genuinely outside \
                     both, or pass --trust-repo-agents to use it as-is (untrusted)"
                ),
                None => tracing::warn!(
                    role = role.as_str(),
                    path = %candidate_path.display(),
                    "ignoring a repo-controlled agent definition for an independent role; move it \
                     to $XDG_CONFIG_HOME/warden/agents/ (or ~/.config/warden/agents/) to use it as \
                     the trusted source, or pass --trust-repo-agents to use it as-is (untrusted)"
                ),
            }
        }
        return Ok(None);
    }

    match try_read_definition(candidate_path, role, adapter).await? {
        Some(definition) => {
            // Issue #26: the moment a repo-controlled definition is actually
            // used for an independent role, this must be impossible to miss
            // -- see the module docs' own "Security: role-asymmetric
            // resolution" section for why this is a `tracing::warn!` *and*
            // (via the caller) a persisted `RunEvent`, not either alone.
            tracing::warn!(
                role = role.as_str(),
                path = %candidate_path.display(),
                "using a repo-controlled agent definition for an independent role \
                 (--trust-repo-agents); this file is committable by the coder and is NOT \
                 trusted the way a genuine user-config definition is"
            );
            // Issue #26 review, LOW: the canonical path was already computed
            // by `user_config_resolves_inside_repo_or_worktrees` for the
            // degraded-user-config case -- reuse it rather than
            // canonicalizing `candidate_path` a second time. For the plain
            // repo-convention case there is no prior canonicalization to
            // reuse, so compute it fresh; the file was just read
            // successfully above, so this can only fail for a reason other
            // than "doesn't exist" via a genuine race.
            let canonical_path = match degraded_user_config_canonical_path {
                Some(canonical) => canonical.to_path_buf(),
                None => canonicalize_best_effort(candidate_path).map_err(|source| {
                    AgentDefinitionError::PathResolutionFailed {
                        path: candidate_path.to_path_buf(),
                        source,
                    }
                })?,
            };
            Ok(Some((
                definition,
                AgentDefinitionSource::UntrustedRepoOverride {
                    path: candidate_path.to_path_buf(),
                    canonical_path,
                },
            )))
        }
        None => Ok(None),
    }
}

/// Attempts to read, parse, and default-fill (B2) the definition at `path`.
/// `Ok(None)` means "no file there" (`io::ErrorKind::NotFound`) -- the only
/// case a caller may treat as "try the next source in precedence order".
/// Every other outcome is either `Ok(Some(..))` (successfully read and
/// parsed) or a typed `Err` naming `path` -- see
/// [`resolve_agent_definition`]'s own docs for why a present-but-broken file
/// must never be silently treated the same as an absent one.
async fn try_read_definition(
    path: &Path,
    role: AgentRole,
    adapter: &impl ToolAdapter,
) -> Result<Option<AgentDefinition>> {
    match tokio::fs::read_to_string(path).await {
        Ok(raw) => {
            let definition =
                parse_agent_definition(&raw).map_err(|source| AgentDefinitionError::Invalid {
                    path: path.to_path_buf(),
                    source,
                })?;
            Ok(Some(apply_default_tools_when_unset(
                definition, role, adapter,
            )))
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(AgentDefinitionError::Read {
            path: path.to_path_buf(),
            source,
        }
        .into()),
    }
}

/// The selected adapter's own default prompt/tools for `role`, used at
/// every consulted location once none of them has a file (issue #24 point
/// 3's "zero markdown at all" UX).
///
/// `AgentDefinition::new` enforces the exact same invariants
/// `parse_agent_definition` does (`warden_core::agent_def`), so an adapter's
/// own default prompt/tools can never produce a definition the parser would
/// have refused. `default_tools` matters as much as `default_prompt` here
/// -- see `ToolAdapter::default_tools`'s own docs for why a `None` grant
/// would leave the "zero .md" UX unable to act at all for a tool like
/// `claude`.
fn adapter_default_definition(
    role: AgentRole,
    adapter: &impl ToolAdapter,
) -> Result<AgentDefinition> {
    Ok(AgentDefinition::new(
        None,
        None,
        adapter.default_tools(role).map(str::to_string),
        None,
        adapter.default_prompt(role),
    )?)
}

/// Resolves the user config directory agent definitions for the reviewer/
/// tester are read from (issue #26): `$XDG_CONFIG_HOME/warden/agents` if
/// `XDG_CONFIG_HOME` is set to a non-blank value, else
/// `$HOME/.config/warden/agents`.
///
/// Hand-rolled rather than a `dirs`/`etcetera` dependency
/// (code-standards.md: "Préférer la stdlib" / "N'inclure que les
/// dépendances réellement utilisées") -- this codebase already has exactly
/// this shape of lookup for `warden_home` (`main.rs::default_warden_home`,
/// `$HOME`-based, no crate), and two env vars don't justify a new
/// dependency just for this one.
///
/// Called exactly once, from `main.rs`, with the result threaded down into
/// [`resolve_agent_definition`] as an explicit parameter rather than read
/// from the environment deep inside this module. This is deliberate for
/// testability: `agent_def.rs`'s own unit tests run in the same process as
/// every other test in this crate, and mutating real process environment
/// variables from there (`std::env::set_var`) would be exactly the
/// "unsafely and with cross-test interference risk under a parallel test
/// runner" hazard `process.rs`'s own tests already call out and avoid (see
/// `spawn_tui_attach_inherits_the_full_parent_environment`'s doc comment)
/// -- a `tempfile::TempDir` passed directly as `user_config_agents_dir`
/// sidesteps it entirely. The CLI's own integration tests (`tests/cli.rs`),
/// which drive the real compiled binary as a separate child process, don't
/// have this hazard and are free to set `XDG_CONFIG_HOME`/`HOME` via
/// `assert_cmd`'s own `Command::env` instead.
pub fn default_user_config_agents_dir() -> Result<PathBuf> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => {
            let home = std::env::var("HOME").map_err(|_| {
                AgentDefinitionError::UserConfigDirUnresolvable {
                    reason: "neither XDG_CONFIG_HOME nor HOME is set".to_string(),
                }
            })?;
            if home.trim().is_empty() {
                return Err(AgentDefinitionError::UserConfigDirUnresolvable {
                    reason: "HOME is set but empty".to_string(),
                }
                .into());
            }
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("warden").join("agents"))
}

/// See [`resolve_agent_definition`]'s own docs (B2): a file that explicitly
/// wrote `tools: ...` keeps exactly that value; a file that omitted the key
/// entirely (`definition.tools == None`) has `adapter.default_tools(role)`
/// merged in instead of being left `None`, so a prompt-only override still
/// runs with a working tool grant.
fn apply_default_tools_when_unset(
    definition: AgentDefinition,
    role: AgentRole,
    adapter: &impl ToolAdapter,
) -> AgentDefinition {
    if definition.tools.is_some() {
        return definition;
    }
    AgentDefinition {
        tools: adapter.default_tools(role).map(str::to_string),
        ..definition
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WardenError;
    use tempfile::TempDir;
    use warden_core::AgentRole;

    struct FakeAdapter;

    impl ToolAdapter for FakeAdapter {
        fn build_command(
            &self,
            _definition: &AgentDefinition,
        ) -> Result<crate::process::AgentCommand> {
            unreachable!("not exercised by these tests")
        }

        fn env_allowlist(&self) -> &'static [&'static str] {
            &[]
        }

        fn extract_findings(
            &self,
            _stdout: &str,
        ) -> warden_core::Result<Vec<warden_core::Finding>> {
            unreachable!("not exercised by these tests")
        }

        fn default_prompt(&self, role: AgentRole) -> &'static str {
            match role {
                AgentRole::Coder => "default coder prompt",
                AgentRole::Reviewer => "default reviewer prompt",
                AgentRole::Tester => "default tester prompt",
            }
        }

        fn default_tools(&self, _role: AgentRole) -> Option<&'static str> {
            Some("fake-default-tools")
        }
    }

    const DEFINITION: &str = "---\nmodel: opus\n---\n\nYou are Warden's reviewer.\n";

    /// Captures `tracing` output emitted while `future` runs, scoped to this
    /// thread only via `tracing::subscriber::set_default` -- used to assert
    /// a `tracing::warn!` this module emits (issue #26 review, MEDIUM/HIGH)
    /// actually fired, without depending on this crate's real stdout/stderr
    /// or any process-global subscriber state (this module's own async unit
    /// tests run concurrently with every other test in this crate, same
    /// cross-test-interference concern `default_user_config_agents_dir`'s
    /// own docs call out for `std::env::set_var`). Sound only because
    /// `#[tokio::test]` uses a current-thread runtime by default: `future`
    /// is polled entirely on this thread, never migrated to another one
    /// while the thread-local subscriber guard is held.
    async fn capture_tracing_output<T>(
        future: impl std::future::Future<Output = T>,
    ) -> (T, String) {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
            type Writer = SharedBuf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .finish();

        let guard = tracing::subscriber::set_default(subscriber);
        let result = future.await;
        drop(guard);

        let output = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        (result, output)
    }

    /// A `user_config_agents_dir` that doesn't exist at all -- the common
    /// case for every test below that isn't specifically exercising the
    /// user-config source, and for a real first-run user who has never
    /// created `~/.config/warden/agents/`. `resolve_agent_definition` must
    /// treat a missing *directory* exactly like a missing *file*
    /// (`io::ErrorKind::NotFound` either way).
    fn no_user_config_dir() -> TempDir {
        TempDir::new().unwrap()
    }

    /// A `warden_home` whose `worktrees/` subdirectory doesn't exist at all
    /// -- the common case for every test below that isn't specifically
    /// exercising the escalated-asymmetry worktrees-root check (owner's
    /// ruling: "the same coder-writable-directory concern also covers a
    /// stale worktree"), and for a real first run of a fresh `warden_home`
    /// that has never created a worktree yet.
    fn no_warden_home_worktrees() -> TempDir {
        TempDir::new().unwrap()
    }

    // -----------------------------------------------------------------
    // Coder: unchanged from issue #24 -- repo convention file, or default.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn coder_loads_and_validates_the_repo_convention_file_when_present() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(repo.path().join(AGENTS_DIR).join("coder.md"), DEFINITION)
            .await
            .unwrap();

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.model.as_deref(), Some("opus"));
        assert_eq!(definition.system_prompt, "You are Warden's reviewer.");
        assert!(matches!(source, AgentDefinitionSource::RepoConvention(_)));
    }

    /// B2 regression (issue #24 review): a convention file that overrides
    /// only the prompt (or any other key) but omits `tools` entirely must
    /// still receive the adapter's own default tools grant -- not `None`,
    /// which for an adapter like `claude` denies every tool call outright
    /// and produces a false convergence (the muzzled agent raises/does
    /// nothing, `decide_next_state` sees a clean cycle). See
    /// `apply_default_tools_when_unset`'s own docs.
    #[tokio::test]
    async fn coder_a_prompt_only_definition_still_gets_the_adapters_default_tools_grant() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(
            repo.path().join(AGENTS_DIR).join("coder.md"),
            "---\n---\nimplement it\n",
        )
        .await
        .unwrap();

        let (definition, _source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.tools.as_deref(), Some("fake-default-tools"));
        assert_eq!(definition.system_prompt, "implement it");
    }

    /// The other half of the B2 fix: a definition that *does* set `tools`
    /// explicitly must keep exactly what it wrote, never overridden by the
    /// adapter's own default.
    #[tokio::test]
    async fn coder_a_definition_that_sets_tools_explicitly_keeps_its_own_value() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(
            repo.path().join(AGENTS_DIR).join("coder.md"),
            "---\ntools: Read, Edit\n---\nimplement it\n",
        )
        .await
        .unwrap();

        let (definition, _source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.tools.as_deref(), Some("Read, Edit"));
    }

    /// The zero-`.md` UX (issue #24): no convention file at all falls back
    /// to the adapter's own default prompt, never an error.
    #[tokio::test]
    async fn coder_falls_back_to_the_adapters_default_prompt_when_no_convention_file_exists() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();

        let (coder, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(coder.system_prompt, "default coder prompt");
        assert_eq!(coder.model, None);
        assert_eq!(source, AgentDefinitionSource::AdapterDefault);
    }

    /// A convention file that exists but is unreadable for a reason other
    /// than "doesn't exist" (here: it's a directory, not a file) must not be
    /// silently treated as absent -- that would hide a real misconfiguration
    /// behind the adapter's default prompt.
    #[tokio::test]
    async fn coder_convention_path_that_is_not_a_regular_file_is_a_read_error_not_a_fallback() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();
        tokio::fs::create_dir_all(
            repo.path().join(AGENTS_DIR).join("coder.md"), // a directory, not a file
        )
        .await
        .unwrap();

        let error = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Read { .. })
        ));
    }

    /// An invalid convention file must surface as `Invalid` (not silently
    /// fall back to the default), naming the file and the parser's own
    /// reason.
    #[tokio::test]
    async fn coder_invalid_convention_file_is_a_typed_error_naming_the_path_and_the_reason() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();
        tokio::fs::create_dir_all(repo.path().join(AGENTS_DIR))
            .await
            .unwrap();
        tokio::fs::write(
            repo.path().join(AGENTS_DIR).join("coder.md"),
            "no frontmatter here\n",
        )
        .await
        .unwrap();

        let error = resolve_agent_definition(
            repo.path(),
            AgentRole::Coder,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Invalid { .. })
        ));
        let rendered = error.to_string();
        assert!(rendered.contains("coder.md"), "{rendered}");
        assert!(rendered.contains("frontmatter"), "{rendered}");
    }

    // -----------------------------------------------------------------
    // Reviewer/Tester: issue #26's role-asymmetric trust.
    // -----------------------------------------------------------------

    async fn write_definition(dir: &Path, role: AgentRole, body: &str) {
        tokio::fs::create_dir_all(dir).await.unwrap();
        tokio::fs::write(dir.join(format!("{}.md", role.as_str())), body)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reviewer_and_tester_load_the_user_config_file_when_present() {
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            let repo = TempDir::new().unwrap();
            let user_config = TempDir::new().unwrap();
            write_definition(user_config.path(), role, DEFINITION).await;

            let (definition, source) = resolve_agent_definition(
                repo.path(),
                role,
                &FakeAdapter,
                user_config.path(),
                no_warden_home_worktrees().path(),
                false,
            )
            .await
            .unwrap();

            assert_eq!(definition.model.as_deref(), Some("opus"));
            assert_eq!(definition.system_prompt, "You are Warden's reviewer.");
            assert!(matches!(source, AgentDefinitionSource::UserConfig(_)));
        }
    }

    /// The core security fix (issue #26): a repo-supplied reviewer/tester
    /// definition must be *ignored* by default -- not read, not erroring
    /// even if it's malformed (never consulted at all) -- unless
    /// `trust_repo_agents` is explicitly set. Without this, a coder could
    /// rewrite the very reviewer/tester judging a *future* run just by
    /// committing `.warden/agents/reviewer.md`.
    ///
    /// Issue #26 review, MEDIUM: "ignored" must not mean "silent" -- a
    /// `tracing::warn!` naming the path must still fire (existence check
    /// only, never opened -- the malformed content above is never read, only
    /// checked to exist) so a user following an older repo-sourced
    /// convention isn't left wondering why the adapter's default prompt ran
    /// instead of theirs.
    #[tokio::test]
    async fn reviewer_and_tester_ignore_the_repo_convention_file_by_default_but_warn_about_it() {
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            let repo = TempDir::new().unwrap();
            let user_config = no_user_config_dir();
            // Deliberately invalid -- proves the repo file is never even
            // opened, not merely lower-precedence than the (absent) user
            // config file.
            write_definition(&repo.path().join(AGENTS_DIR), role, "no frontmatter here\n").await;

            let (result, logs) = capture_tracing_output(resolve_agent_definition(
                repo.path(),
                role,
                &FakeAdapter,
                user_config.path(),
                no_warden_home_worktrees().path(),
                false,
            ))
            .await;
            let (definition, source) = result.unwrap();

            assert_eq!(source, AgentDefinitionSource::AdapterDefault);
            assert_eq!(
                definition.system_prompt,
                if role == AgentRole::Reviewer {
                    "default reviewer prompt"
                } else {
                    "default tester prompt"
                }
            );

            let expected_path = repo
                .path()
                .join(AGENTS_DIR)
                .join(format!("{}.md", role.as_str()));
            assert!(
                logs.contains("ignoring a repo-controlled agent definition"),
                "{logs:?}"
            );
            assert!(
                logs.contains(&expected_path.display().to_string()),
                "{logs:?}"
            );
        }
    }

    /// The opt-in escape hatch: with `trust_repo_agents: true` and no
    /// user-config file, the repo's own convention file is used -- but
    /// surfaced as [`AgentDefinitionSource::UntrustedRepoOverride`], never
    /// indistinguishable from a trusted resolution.
    #[tokio::test]
    async fn trust_repo_agents_falls_back_to_the_repo_file_when_no_user_config_file_exists() {
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            let repo = TempDir::new().unwrap();
            let user_config = no_user_config_dir();
            write_definition(&repo.path().join(AGENTS_DIR), role, DEFINITION).await;

            let (definition, source) = resolve_agent_definition(
                repo.path(),
                role,
                &FakeAdapter,
                user_config.path(),
                no_warden_home_worktrees().path(),
                true,
            )
            .await
            .unwrap();

            assert_eq!(definition.system_prompt, "You are Warden's reviewer.");
            let expected_path = repo
                .path()
                .join(AGENTS_DIR)
                .join(format!("{}.md", role.as_str()));
            // Compared via `.canonicalize()` (not a raw string/`PathBuf`
            // equality against `expected_path` itself) since a tempdir's own
            // path may itself sit behind a symlink (e.g. macOS's `/var` ->
            // `/private/var`) -- both sides must go through the exact same
            // resolution to agree.
            assert_eq!(
                source,
                AgentDefinitionSource::UntrustedRepoOverride {
                    path: expected_path.clone(),
                    canonical_path: expected_path.canonicalize().unwrap(),
                }
            );
        }
    }

    /// Precedence (issue #26): when both a user-config file and a repo file
    /// exist for the same role, the user-config file always wins -- the repo
    /// file is a fallback for when nothing is configured yet, never a way to
    /// shadow a value the user deliberately set for themselves.
    #[tokio::test]
    async fn user_config_file_wins_over_the_repo_file_even_with_trust_repo_agents() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            user_config.path(),
            AgentRole::Reviewer,
            "---\n---\nfrom user config\n",
        )
        .await;
        write_definition(
            &repo.path().join(AGENTS_DIR),
            AgentRole::Reviewer,
            "---\n---\nfrom the repo\n",
        )
        .await;

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            true,
        )
        .await
        .unwrap();

        assert_eq!(definition.system_prompt, "from user config");
        assert!(matches!(source, AgentDefinitionSource::UserConfig(_)));
    }

    /// `trust_repo_agents: true` with *neither* a user-config file nor a
    /// repo file still falls all the way through to the adapter's own
    /// default -- the flag only ever adds a fallback location, never removes
    /// the final one.
    #[tokio::test]
    async fn trust_repo_agents_still_falls_back_to_the_adapter_default_when_nothing_exists() {
        let repo = TempDir::new().unwrap();
        let user_config = no_user_config_dir();

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Tester,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            true,
        )
        .await
        .unwrap();

        assert_eq!(definition.system_prompt, "default tester prompt");
        assert_eq!(source, AgentDefinitionSource::AdapterDefault);
    }

    /// A user-config file that exists but is unreadable for a reason other
    /// than "doesn't exist" must be a typed error, exactly like the coder's
    /// own convention file -- never silently treated as absent.
    #[tokio::test]
    async fn reviewer_user_config_path_that_is_not_a_regular_file_is_a_read_error() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        tokio::fs::create_dir_all(user_config.path().join("reviewer.md")) // a directory
            .await
            .unwrap();

        let error = resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Read { .. })
        ));
    }

    /// An invalid user-config file must surface as `Invalid`, naming the
    /// path and the parser's own reason -- same rule as the coder's
    /// convention file, applied to the trusted source.
    #[tokio::test]
    async fn reviewer_invalid_user_config_file_is_a_typed_error_naming_the_path_and_the_reason() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            user_config.path(),
            AgentRole::Tester,
            "no frontmatter here\n",
        )
        .await;

        let error = resolve_agent_definition(
            repo.path(),
            AgentRole::Tester,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            WardenError::AgentDefinition(AgentDefinitionError::Invalid { .. })
        ));
        let rendered = error.to_string();
        assert!(rendered.contains("tester.md"), "{rendered}");
        assert!(rendered.contains("frontmatter"), "{rendered}");
    }

    /// B2 applies identically to the user-config source: a prompt-only
    /// override still gets the adapter's own default tools grant.
    #[tokio::test]
    async fn reviewer_user_config_prompt_only_definition_still_gets_the_adapters_default_tools() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            user_config.path(),
            AgentRole::Reviewer,
            "---\n---\nreview it\n",
        )
        .await;

        let (definition, _source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.tools.as_deref(), Some("fake-default-tools"));
        assert_eq!(definition.system_prompt, "review it");
    }

    // -----------------------------------------------------------------
    // Issue #26 review, HIGH: a "trusted" user-config dir that actually
    // resolves inside the repo under review is degraded to untrusted.
    // -----------------------------------------------------------------

    /// The core of the HIGH fix: `user_config_agents_dir` pointing *inside*
    /// `repo_path` (the coder-controlled-`XDG_CONFIG_HOME` attack) must never
    /// be treated as [`AgentDefinitionSource::UserConfig`] -- with the flag
    /// off, it is ignored, never silently read as trusted.
    ///
    /// Issue #26 review, LOW: the warning text here must be the
    /// *degraded-user-config* message, distinct from the plain
    /// repo-convention-file one (`reviewer_and_tester_ignore_the_repo_convention_file_by_default_but_warn_about_it`
    /// pins that shape) -- "move it to `$XDG_CONFIG_HOME/warden/agents/`" is
    /// a no-op for a file that is *already* there, just resolving somewhere
    /// unexpected.
    #[tokio::test]
    async fn a_user_config_dir_resolving_inside_the_repo_is_ignored_by_default_and_warns_with_the_degraded_message(
    ) {
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            let repo = TempDir::new().unwrap();
            // The attack this fix closes: XDG_CONFIG_HOME pointed at a
            // directory inside the repo the coder controls (e.g. via a
            // committed `.envrc`).
            let malicious_user_config_dir = repo.path().join(".config");
            write_definition(
                &malicious_user_config_dir.join("warden").join("agents"),
                role,
                "---\n---\nfrom the fake user config (actually the repo)\n",
            )
            .await;

            let (result, logs) = capture_tracing_output(resolve_agent_definition(
                repo.path(),
                role,
                &FakeAdapter,
                &malicious_user_config_dir.join("warden").join("agents"),
                no_warden_home_worktrees().path(),
                false,
            ))
            .await;
            let (definition, source) = result.unwrap();

            assert_eq!(source, AgentDefinitionSource::AdapterDefault);
            assert_eq!(
                definition.system_prompt,
                if role == AgentRole::Reviewer {
                    "default reviewer prompt"
                } else {
                    "default tester prompt"
                },
                "a degraded user-config source must never be read as trusted, even though a \
                 file genuinely exists there"
            );
            assert!(
                logs.contains(
                    "ignoring a reviewer/tester definition that looked like the trusted user \
                     config source"
                ),
                "{logs:?}"
            );
            assert!(
                !logs.contains("move it to $XDG_CONFIG_HOME/warden/agents/"),
                "the degraded-user-config case must not get the plain repo-convention advice, \
                 which is a no-op for a file already at that exact location: {logs:?}"
            );
        }
    }

    /// The other half: with `trust_repo_agents: true`, the same degraded
    /// user-config path is actually used -- but surfaced as
    /// [`AgentDefinitionSource::UntrustedRepoOverride`] naming that exact
    /// path, with a `tracing::warn!`, never as
    /// [`AgentDefinitionSource::UserConfig`].
    #[tokio::test]
    async fn a_user_config_dir_resolving_inside_the_repo_is_used_as_untrusted_when_trusted() {
        let repo = TempDir::new().unwrap();
        let malicious_user_config_dir = repo.path().join(".config");
        let malicious_agents_dir = malicious_user_config_dir.join("warden").join("agents");
        write_definition(
            &malicious_agents_dir,
            AgentRole::Reviewer,
            "---\n---\nfrom the fake user config (actually the repo)\n",
        )
        .await;

        let (result, logs) = capture_tracing_output(resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            &malicious_agents_dir,
            no_warden_home_worktrees().path(),
            true,
        ))
        .await;
        let (definition, source) = result.unwrap();

        assert_eq!(
            definition.system_prompt,
            "from the fake user config (actually the repo)"
        );
        let expected_path = malicious_agents_dir.join("reviewer.md");
        assert_eq!(
            source,
            AgentDefinitionSource::UntrustedRepoOverride {
                path: expected_path.clone(),
                canonical_path: expected_path.canonicalize().unwrap(),
            }
        );
        assert!(logs.contains("NOT trusted"), "{logs:?}");
        assert!(
            logs.contains(&expected_path.display().to_string()),
            "{logs:?}"
        );
    }

    /// A symlinked `<role>.md` pointing back into the repo under review must
    /// be caught by the same check even when `user_config_agents_dir` itself
    /// is a perfectly ordinary, outside-the-repo directory -- the containment
    /// check canonicalizes the resolved `<role>.md` path itself, not just its
    /// parent directory.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_user_config_file_pointing_into_the_repo_is_degraded_to_untrusted() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            &repo.path().join(AGENTS_DIR),
            AgentRole::Reviewer,
            "---\n---\nfrom the repo, via a symlink\n",
        )
        .await;

        std::os::unix::fs::symlink(
            repo.path().join(AGENTS_DIR).join("reviewer.md"),
            user_config.path().join("reviewer.md"),
        )
        .unwrap();

        let (result, logs) = capture_tracing_output(resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            true,
        ))
        .await;
        let (definition, source) = result.unwrap();

        assert_eq!(definition.system_prompt, "from the repo, via a symlink");
        match &source {
            AgentDefinitionSource::UntrustedRepoOverride {
                path,
                canonical_path,
            } => {
                // Issue #26 review, LOW: this is exactly the case the fix
                // exists for -- `path` is the symlink itself (looks like a
                // perfectly ordinary user-config location), `canonical_path`
                // must name where it actually resolves to (the repo's own
                // file, symlink followed).
                assert_eq!(path, &user_config.path().join("reviewer.md"));
                assert_eq!(
                    canonical_path,
                    &repo
                        .path()
                        .join(AGENTS_DIR)
                        .join("reviewer.md")
                        .canonicalize()
                        .unwrap()
                );
            }
            other => panic!("expected UntrustedRepoOverride, got {other:?}"),
        }
        assert!(logs.contains("NOT trusted"), "{logs:?}");
    }

    /// A genuine user-config directory that sits nowhere near the repo must
    /// be entirely unaffected by the HIGH fix -- the containment check must
    /// not produce false positives for the overwhelmingly common case.
    #[tokio::test]
    async fn a_user_config_dir_genuinely_outside_the_repo_is_unaffected_by_the_containment_check() {
        let repo = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            user_config.path(),
            AgentRole::Reviewer,
            "---\n---\ngenuinely trusted\n",
        )
        .await;

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            no_warden_home_worktrees().path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.system_prompt, "genuinely trusted");
        assert!(matches!(source, AgentDefinitionSource::UserConfig(_)));
    }

    // -----------------------------------------------------------------
    // Owner's ruling on the escalated asymmetry: a user-config source
    // resolving under `<warden_home>/worktrees/` (a stale worktree from a
    // crashed run) must be degraded exactly like one resolving inside the
    // repo itself.
    // -----------------------------------------------------------------

    /// The core of the ruling: `user_config_agents_dir` pointing *inside*
    /// `<warden_home>/worktrees/` -- e.g. a stale worktree left behind by a
    /// crashed run, referenced by a committed `.envrc` a human's shell still
    /// picks up on the *next* invocation -- must never be treated as
    /// [`AgentDefinitionSource::UserConfig`], even though it sits nowhere
    /// near `repo_path` itself.
    #[tokio::test]
    async fn a_user_config_dir_resolving_inside_warden_homes_worktrees_is_ignored_by_default_and_warns(
    ) {
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            let repo = TempDir::new().unwrap();
            let warden_home = TempDir::new().unwrap();
            // Models a stale worktree from a crashed prior run: a directory
            // under `<warden_home>/worktrees/` that a committed `.envrc`
            // still points `XDG_CONFIG_HOME` at.
            let stale_worktree_user_config = warden_home
                .path()
                .join("worktrees")
                .join("crashed-run-id")
                .join("coder")
                .join(".config");
            write_definition(
                &stale_worktree_user_config.join("warden").join("agents"),
                role,
                "---\n---\nfrom a stale worktree, not the trusted user config\n",
            )
            .await;

            let (result, logs) = capture_tracing_output(resolve_agent_definition(
                repo.path(),
                role,
                &FakeAdapter,
                &stale_worktree_user_config.join("warden").join("agents"),
                warden_home.path(),
                false,
            ))
            .await;
            let (definition, source) = result.unwrap();

            assert_eq!(
                source,
                AgentDefinitionSource::AdapterDefault,
                "a user-config source resolving inside <warden_home>/worktrees/ must never be \
                 read as trusted, even though a file genuinely exists there"
            );
            assert_eq!(
                definition.system_prompt,
                if role == AgentRole::Reviewer {
                    "default reviewer prompt"
                } else {
                    "default tester prompt"
                }
            );
            assert!(
                logs.contains(
                    "ignoring a reviewer/tester definition that looked like the trusted user \
                     config source"
                ),
                "{logs:?}"
            );
        }
    }

    /// The other half of the ruling: with `trust_repo_agents: true`, the
    /// same stale-worktree user-config path is actually used -- but
    /// surfaced as [`AgentDefinitionSource::UntrustedRepoOverride`], never
    /// as [`AgentDefinitionSource::UserConfig`].
    #[tokio::test]
    async fn a_user_config_dir_resolving_inside_warden_homes_worktrees_is_used_as_untrusted_when_trusted(
    ) {
        let repo = TempDir::new().unwrap();
        let warden_home = TempDir::new().unwrap();
        let stale_worktree_user_config = warden_home
            .path()
            .join("worktrees")
            .join("crashed-run-id")
            .join("coder")
            .join(".config");
        let stale_agents_dir = stale_worktree_user_config.join("warden").join("agents");
        write_definition(
            &stale_agents_dir,
            AgentRole::Reviewer,
            "---\n---\nfrom a stale worktree, not the trusted user config\n",
        )
        .await;

        let (result, logs) = capture_tracing_output(resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            &stale_agents_dir,
            warden_home.path(),
            true,
        ))
        .await;
        let (definition, source) = result.unwrap();

        assert_eq!(
            definition.system_prompt,
            "from a stale worktree, not the trusted user config"
        );
        let expected_path = stale_agents_dir.join("reviewer.md");
        assert_eq!(
            source,
            AgentDefinitionSource::UntrustedRepoOverride {
                path: expected_path.clone(),
                canonical_path: expected_path.canonicalize().unwrap(),
            }
        );
        assert!(logs.contains("NOT trusted"), "{logs:?}");
    }

    /// A `user_config_agents_dir` under a *different* `warden_home`'s own
    /// worktrees (or nowhere near this one's at all) must be entirely
    /// unaffected -- the ruling only extends to *this* run's own
    /// `warden_home`, never every worktree tree on disk.
    #[tokio::test]
    async fn a_user_config_dir_outside_this_warden_homes_worktrees_is_unaffected() {
        let repo = TempDir::new().unwrap();
        let warden_home = TempDir::new().unwrap();
        let user_config = TempDir::new().unwrap();
        write_definition(
            user_config.path(),
            AgentRole::Reviewer,
            "---\n---\ngenuinely trusted\n",
        )
        .await;

        let (definition, source) = resolve_agent_definition(
            repo.path(),
            AgentRole::Reviewer,
            &FakeAdapter,
            user_config.path(),
            warden_home.path(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(definition.system_prompt, "genuinely trusted");
        assert!(matches!(source, AgentDefinitionSource::UserConfig(_)));
    }
}
