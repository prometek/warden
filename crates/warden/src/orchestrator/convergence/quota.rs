use super::super::*;

impl Orchestrator {
    pub(super) async fn persist_quota_suspension(
        &self,
        run_id: &str,
        config: &RunConfig,
        continuation: &ConvergenceContinuation,
        resets_at: i64,
    ) -> Result<RunState> {
        let run =
            db::get_run(&self.pool, run_id)
                .await?
                .ok_or_else(|| WardenError::RunNotFound {
                    run_id: run_id.to_string(),
                })?;
        let state = RunState::AwaitingQuotaReset { resets_at };
        run.state.validate_transition(state, run.total_steps)?;
        let execution_context = self.run_execution_context.as_ref().ok_or_else(|| {
            WardenError::MissingQuotaExecutionContext {
                run_id: run_id.to_string(),
            }
        })?;
        let config_json = super::continuation::encode_run_config(
            config,
            execution_context,
            self.quota_anticipation_threshold,
        )?;
        let state_json = super::continuation::encode_convergence_state(continuation)?;
        db::suspend_run_with_quota_continuation(
            &self.pool,
            run_id,
            resets_at,
            &config_json,
            &state_json,
        )
        .await?;
        Ok(state)
    }
}
