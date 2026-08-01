use super::*;

/// Selects findings that triggered a cycle.
pub(super) async fn select_prior_findings(
    pool: &SqlitePool,
    ci_seeded_findings: Vec<Finding>,
    previous_cycle_id: Option<&str>,
) -> Result<Vec<Finding>> {
    if !ci_seeded_findings.is_empty() {
        return Ok(ci_seeded_findings);
    }
    match previous_cycle_id {
        Some(prev_cycle_id) => db::list_findings_for_cycle(pool, prev_cycle_id).await,
        None => Ok(Vec::new()),
    }
}
