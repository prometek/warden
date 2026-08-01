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
