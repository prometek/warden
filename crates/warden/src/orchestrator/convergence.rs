//! The main convergence-loop driver: [`Orchestrator::run_convergence_loop`]
//! alternates coder / review+test cycles until convergence, the cycle
//! budget is exhausted, or cancellation fires.

use super::*;

mod driver;
mod prior_findings;
mod quota;

use prior_findings::select_prior_findings;

#[cfg(test)]
mod tests;
