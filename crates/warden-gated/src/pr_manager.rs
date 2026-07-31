//! PR Manager (ADR-0007, issue #4): the three-action PR lifecycle
//! (`OpenDraft` / `PostCycleUpdate` / `Finalize`) that `warden-gated` owns
//! exclusively, plus the linked-issue detection and commit-trailer
//! formatting that support it.
//!
//! This module never talks to a PR provider's API/CLI directly -- that's
//! [`PrProvider`]'s job, implemented by `gh_provider::GhProvider` (GitHub)
//! today. A `glab`-backed implementation of the same trait is the intended
//! drop-in extension point for GitLab (deferred: GitHub is the priority
//! provider per Architecture.md's roadmap).
//!
//! Security boundary (ADR-0002/0006/0007, unchanged by this module):
//! `OpenDraft` and `PostCycleUpdate` only ever push a branch skeleton or
//! talk to the PR provider's *metadata* (title/body/comments) -- never
//! business code. `Finalize` is the only action that pushes real content,
//! and it does so by calling the exact same `gate::verify_and_authorize` +
//! `push::push_to_origin` path the git-push gate itself uses (see
//! `serve::handle_push_notification_line`) -- never a separate, weaker
//! check.

use std::path::Path;

use sqlx::SqlitePool;
use tokio::process::Command;
use warden_core::{Finding, FindingSource};

use crate::error::{GatedError, Result};
use crate::gate::verify_and_authorize;
use crate::push;
use crate::verify::{GateBlockReason, GateDecision};

mod content;
mod git_validation;
mod provider;
mod workflow;

pub use content::*;
pub use provider::*;
pub use workflow::*;

use git_validation::skeleton_diff_against_base;
pub(crate) use git_validation::{fetch_branch, remote_branch_head, EMPTY_TREE_SHA};

#[cfg(test)]
use content::MAX_GENERATED_TITLE_LEN;
#[cfg(test)]
use workflow::finalize_pr_body;

#[cfg(test)]
mod tests;
