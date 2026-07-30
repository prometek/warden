//! Workflow step invocation (issue #73, trio-unification follow-up):
//! [`Orchestrator::run_producer`] for `workflow.steps[0]`, and
//! [`Orchestrator::run_gated_step`] for every step after it -- one uniform
//! body for the built-in reviewer/tester and any custom role alike. No step
//! is special-cased by role name in either function.

use super::agent_run::SandboxGuard;
use super::diff::{read_diff, read_head_commit};
use super::gate_tail::protect_cycle_commit;
use super::tampering::agent_definition_tampering_finding;
use super::*;

/// Bounds how much of an agent's stderr is embedded in an error message —
/// full output is already logged via `tracing` before this is constructed;
/// this is just what surfaces in `Display`/CLI output.
const MAX_ERROR_STDERR_LEN: usize = 2000;

fn truncate_for_error(stderr: &str) -> String {
    if stderr.len() <= MAX_ERROR_STDERR_LEN {
        return stderr.to_string();
    }
    // Truncate on a char boundary — stderr is arbitrary agent output and
    // may contain multi-byte UTF-8, so a byte-offset slice could panic.
    let boundary = stderr
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= MAX_ERROR_STDERR_LEN)
        .last()
        .unwrap_or(0);
    format!("{}… (truncated)", &stderr[..boundary])
}

/// "The cycle's e2e test succeeded" (ADR-0009: evidence is captured "après
/// le succès du test e2e"), inferred as "the evidence-capturing step itself
/// raised no blocking finding" -- there's no separate pass/fail signal in
/// the findings protocol, so absence of a blocking finding sourced from
/// `role` is the only available proxy. Generalized off `role` (issue #73
/// review, finding F2) rather than hardcoding `FindingSource::role("tester")`,
/// since the step this now watches is whichever one declared
/// `captures_evidence: true`, not necessarily one named `"tester"`.
fn step_succeeded(role: &Role, findings: &[Finding]) -> bool {
    !findings.iter().any(|finding| {
        finding.source == warden_core::FindingSource::role(role.as_str())
            && finding.severity == warden_core::Severity::Blocking
    })
}

/// The environment variable *names* forwarded to a `type: hook` workflow
/// step's command (issue #79 review, HIGH -- security).
///
/// # Trust model
///
/// **`run:` is repo-authored, not operator-authored.** A `type: hook` step's
/// `run` line is declared in `<repo>/.warden/workflow.yaml` -- a file that
/// lives in, and can be committed by, the repo under review (a prior run's
/// own coder can write it), the same trust class as `.warden/hooks.toml`
/// (`crate::hook_config`'s own "Trust model" docs, which this mirrors).
/// Before issue #79, `workflow.yaml` could only *name* an agent definition;
/// it can now carry arbitrary shell, honoured **by default**, with no opt-in
/// flag -- the same "already running this repo's coder, already running a
/// `Makefile`/npm `postinstall`'s commands" posture `hook_config`'s own docs
/// argue for `.warden/hooks.toml`. This allowlist is what *bounds the blast
/// radius* of that already-accepted trust decision, not what makes running
/// repo-authored shell safe in the first place.
///
/// **Deliberately narrower than a lifecycle `CommandHook`'s own
/// `full_env_allowlist`** (`crate::hook::CommandHook`'s own "Environment"
/// docs, which forwards the operator's complete environment). The two
/// differ because what each runs *against* differs: a `CommandHook` runs
/// against the operator's own checkout (`HookContext::repo_path`); a `type:
/// hook` workflow step runs against a worktree checked out at **this
/// cycle's coder-authored commit** -- the first path in this codebase where
/// agent-authored code (a `build.rs`, an `npm test` script, a `Makefile`
/// target, ...) executes with *some* environment at all. Forwarding the
/// operator's full environment here would hand that code
/// `SSH_AUTH_SOCK`/`GH_TOKEN`/`AWS_*`/`ANTHROPIC_API_KEY` -- exactly the
/// exposure an agent subprocess is already denied via its own narrow
/// `ToolAdapter::env_allowlist`.
///
/// **Under `--isolation docker`, this allowlist is not the whole story** --
/// it bounds *environment variables* only. `DockerSandbox` unconditionally
/// bind-mounts the role's own worktree read-write, the base repo's `.git`
/// read-write, and the host's `~/.claude` read-only
/// (`warden_sandbox::docker`'s own module docs), and applies no egress
/// filtering (Docker's default bridge network) -- none of that is related
/// to `env_allowlist` at all, and none of it is closed by this list. A
/// `type: hook` step's command inherits exactly the same mount/network
/// surface every `type: agent` step (the existing tester, say) already has
/// under docker -- accepted parity with an existing exposure, not a new one
/// this issue introduces, but not a claim that this allowlist alone makes a
/// hook step's command sandboxed from host credentials.
///
/// Kept to what a deterministic build/lint/test command needs to run at
/// all. `PATH` needs no entry here for `LocalSandbox` -- it is always
/// forwarded regardless of any allowlist (`LocalSandbox::execute`'s own
/// docs) -- but under `--isolation docker` there is no host `PATH` to
/// forward at all; the container image's own `PATH` applies
/// (`warden_sandbox::docker` never sets `-e PATH=`). `HOME`/`LANG`/`TERM`
/// cover a well-behaved POSIX toolchain on either backend. `CARGO_HOME`/
/// `RUSTUP_HOME` cover a non-default Rust toolchain layout on
/// `LocalSandbox` -- harmless to list when unset, since an
/// allowlisted-but-unset variable is simply never forwarded
/// (`LocalSandbox::execute`'s own docs). **Known docker caveat**: under
/// `--isolation docker`, an operator who *does* set a non-default
/// `CARGO_HOME`/`RUSTUP_HOME` still has it forwarded verbatim as a **host**
/// path, which does not exist inside the container -- `cargo`/`rustup`
/// commands can then fail or misbehave rather than silently falling back to
/// their container-default location. Not special-cased per backend here
/// (the sandbox seam is deliberately backend-agnostic at this call site);
/// an operator hitting this should use a `type: agent` step instead, or
/// wait for a future per-step `env:` override (see below).
///
/// Verified empirically against this crate's own shipped example
/// (`examples/workflows/with-lint-hook/workflow.yaml`, `cargo fmt --all --
/// --check`, `LocalSandbox`) with nothing but `PATH`/`HOME` set in the
/// invoking shell.
///
/// No per-step `env:` override ships yet -- a workflow step whose command
/// needs something beyond this list fails with a clear, actionable "command
/// not found"/tool-specific error, not a silent misconfiguration;
/// broadening what a step can forward is a future, explicit opt-in
/// (a `workflow.yaml` `env:` key with the same parse-time validation rigour
/// as `type`/`run`), never a default this list should silently grow into.
const WORKFLOW_STEP_ENV_ALLOWLIST: &[&str] = &["HOME", "LANG", "TERM", "CARGO_HOME", "RUSTUP_HOME"];

/// One blocking [`Finding`] sourced as `role`'s own `FindingSource` (issue
/// #79) -- the shape both a policy-denied and a non-zero-exit `type: hook`
/// step verdict reduce to, aggregated into the convergence loop exactly like
/// a `type: agent` step's own findings already are.
fn blocking_step_finding(role: &Role, reason: String) -> Finding {
    Finding {
        source: warden_core::FindingSource::role(role.as_str()),
        severity: warden_core::Severity::Blocking,
        file: None,
        description: reason,
        action: Some(
            "fix the failing deterministic check; the pipeline retries once corrected".to_string(),
        ),
    }
}

impl Orchestrator {
    /// Invokes `workflow.steps[0]` -- the pipeline's producer (the coder in
    /// the built-in default workflow) -- for one cycle: its own worktree,
    /// its own agent spawn, the commit/diff it produced, and the tampering
    /// check against the run's original agent-definition snapshot.
    ///
    /// **Issue #73 (trio-unification follow-up)**: `invocation.role` is the
    /// producer step's own open [`Role`] rather than a hardcoded
    /// `AgentRole::Coder` -- a workflow's first step is not necessarily
    /// named `"coder"`. The one genuine, structural invariant this function
    /// embodies (documented, not a role-name check): **only the producer
    /// step may write a new commit** -- it is the one role whose payload
    /// carries `intent` rather than `target_commit`/`diff`, and the one
    /// role `run_convergence_loop` ever calls this for (`workflow.steps[0]`,
    /// a positional fact enforced by [`warden_core::Workflow::parse_yaml`]
    /// requiring the first step to be a plain pass-through).
    pub(super) async fn run_producer<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: ProducerInvocation<'_>,
    ) -> Result<ProducerCycleResult> {
        let ProducerInvocation {
            run_id,
            cycle_id,
            cycle_number,
            config,
            role,
            agent,
            env_allowlist,
            worktree_manager,
            base_commit,
            run_agent_definition_snapshot,
            prior_findings,
            cancel,
        } = invocation;

        let worktree = worktree_manager
            .create(run_id, role.as_str(), base_commit)
            .await?;
        db::set_cycle_worktree_path(
            &self.pool,
            cycle_id,
            role.as_str(),
            &worktree.path().display().to_string(),
        )
        .await?;

        // ADR-0012: resolved right after the worktree is created (before
        // the producer runs), so it's a concrete SHA rather than the
        // possibly ambiguous `base_commit` ref (e.g. the literal string
        // `"HEAD"` on a run's first cycle) -- needed below to compute the
        // diff this cycle's producer introduces, once it has run.
        let base_commit_sha = read_head_commit(worktree.path()).await?;

        // ADR-0013: the producer's own definition (system prompt), the run
        // intent, and -- A2 -- the findings it is being asked to fix. No
        // `target_commit`/`diff`: this very worktree is already checked out
        // at that commit, so the producer can `git diff` for itself rather
        // than be handed a copy of what's on its own disk.
        let stdin_payload = warden_core::build_producer_input_json(
            role.as_str(),
            &agent.system_prompt,
            config.intent.clone(),
            prior_findings.to_vec(),
        )?;
        let outcome = self
            .run_agent(
                cycle_id,
                role,
                true,
                runner,
                &agent.command,
                env_allowlist,
                worktree.path(),
                &config.repo_path,
                &config.warden_home.join("worktrees").join(run_id),
                &agent.trusted_arg_values,
                stdin_payload,
                cancel,
            )
            .await?;

        // M2: a producer that exits non-zero has not reliably produced a
        // commit worth reviewing — `read_head_commit` below would just
        // return the unchanged base commit, silently making the loop look
        // like a no-op success. Fail the run explicitly instead.
        if outcome.exit_code != 0 {
            tracing::warn!(
                run_id,
                cycle_id,
                role = role.as_str(),
                exit_code = outcome.exit_code,
                stderr = %outcome.stderr,
                "producer step exited with a non-zero status; failing the run"
            );
            // Write-ahead (ADR-0004): persist Failed before returning the
            // error to the caller.
            self.transition(run_id, RunState::Failed).await?;
            // A TUI observer must see a terminal event rather than the
            // stream simply going silent -- this is the one place the run
            // ends without ever reaching `run_convergence_loop`'s own
            // `RunFinished` publish at the bottom of its loop.
            self.publish_event(RunEvent::RunFinished {
                final_state: RunState::Failed.as_str().to_string(),
            })
            .await?;
            if let Err(error) = worktree.remove().await {
                tracing::warn!(%error, "failed to clean up producer worktree after a failed run");
            }
            return Err(WardenError::CoderFailed {
                run_id: run_id.to_string(),
                cycle_id: cycle_id.to_string(),
                exit_code: outcome.exit_code,
                stderr: truncate_for_error(&outcome.stderr),
            });
        }

        let new_commit = read_head_commit(worktree.path()).await?;

        // ADR-0012: computed while the worktree still exists (both commits
        // are reachable from it, since worktrees share the main repo's
        // object store) -- this is what every gated step's
        // `AgentInputMessage::diff` carries.
        let diff = read_diff(worktree.path(), &base_commit_sha, &new_commit).await?;

        // Issue #30 (review, HIGH): re-resolves the built-in trio's raw
        // definition bytes through a throwaway `git worktree` checkout of
        // `new_commit` -- deliberately not this cycle's own producer
        // worktree working directory, which is mutable and not what
        // actually propagates forward -- and compares each against
        // `run_agent_definition_snapshot` (the run's true original start,
        // captured once in `run_convergence_loop`) -- see
        // `agent_definition_tampering_finding`'s own docs for the full
        // rationale.
        let definition_tampering_finding = agent_definition_tampering_finding(
            worktree_manager,
            run_id,
            &new_commit,
            run_agent_definition_snapshot,
        )
        .await?;

        // M4: protect the commit from `git gc` (worktrees share the main
        // repo's object store, so this commit becomes unreachable garbage
        // the moment its worktree is removed) and persist its SHA so it
        // stays discoverable — both purely local git/DB operations, no
        // push, no remote (that's Phase 3's git gate).
        protect_cycle_commit(&config.repo_path, run_id, cycle_number, &new_commit).await?;
        db::set_cycle_commit_sha(&self.pool, cycle_id, &new_commit).await?;

        if let Err(error) = worktree.remove().await {
            tracing::warn!(%error, "failed to clean up producer worktree after cycle");
        }

        Ok(ProducerCycleResult {
            commit: new_commit,
            diff,
            definition_tampering_finding,
        })
    }

    /// Invokes a single **gated** workflow step -- any step but the
    /// producer (`workflow.steps[0]`), whether that's the built-in
    /// reviewer/tester, a custom `type: agent` role like `techlead`, or a
    /// `type: hook` step (issue #79). Dispatches on
    /// [`GatedStepInvocation::kind`]: an agent-kind step spawns a subprocess
    /// ([`Orchestrator::run_gated_agent_step`]); a hook-kind step runs a
    /// deterministic command instead
    /// ([`Orchestrator::run_gated_hook_step`]). Both return the exact same
    /// `Vec<Finding>` shape, aggregated into the convergence loop identically
    /// regardless of which kind actually ran.
    pub(super) async fn run_gated_step<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: GatedStepInvocation<'_>,
    ) -> Result<Vec<Finding>> {
        match invocation.kind {
            warden_core::StepKind::Agent => self.run_gated_agent_step(runner, invocation).await,
            warden_core::StepKind::Hook => self.run_gated_hook_step(invocation).await,
        }
    }

    /// The `type: agent` half of [`Orchestrator::run_gated_step`]'s dispatch:
    /// its own worktree, its own agent spawn, its own findings extraction,
    /// validated against its own open [`Role`] -- no role name ever
    /// branches this function's own behaviour.
    ///
    /// **One narrow, documented exception, positional/functional rather
    /// than a role-name check:** `scope` may only be
    /// [`warden_core::ReviewScope::Correctif`] for `invocation.step_index
    /// == 1` (the first gated step) -- decision #37 Q2's scoped-re-review
    /// optimization is a pipeline mechanic tied to *position*, not to a role
    /// named `"reviewer"`; `run_convergence_loop` is the only caller that
    /// ever sets it, and this is a defensive re-check against a future
    /// caller doing so incorrectly, mirroring this crate's existing
    /// "constructor invariant == defensive re-check" convention.
    ///
    /// Evidence capture (ADR-0009, issue #7) no longer keys on a literal
    /// role name either (issue #73 review, finding F2): it fires whenever
    /// `invocation.captures_evidence` is set, a property `run_convergence_loop`
    /// reads straight off this step's own `workflow.yaml` declaration
    /// (`WorkflowStep::captures_evidence`) -- `true` for the built-in
    /// default's tester step (`Workflow::builtin_default`), for strict
    /// retro-compat, and opt-in for any custom workflow's own step.
    async fn run_gated_agent_step<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: GatedStepInvocation<'_>,
    ) -> Result<Vec<Finding>> {
        let GatedStepInvocation {
            run_id,
            cycle_id,
            cycle_number,
            step_index,
            role,
            kind: _,
            agent,
            run: _,
            env_allowlist,
            worktree_manager,
            commit,
            diff,
            prior_findings,
            scope,
            captures_evidence,
            config,
            cancel,
        } = invocation;
        let agent = agent.expect(
            "run_gated_agent_step is only reached for StepKind::Agent, which always carries a \
             resolved agent -- ResolvedAgents::resolve's own invariant",
        );

        if scope == warden_core::ReviewScope::Correctif && step_index != 1 {
            return Err(WardenError::Core(
                warden_core::CoreError::MalformedAgentInput(format!(
                    "step {step_index} ({role}) cannot be invoked with a scoped (\"correctif\") \
                     review -- only the first gated step (index 1) can be scoped"
                )),
            ));
        }

        let worktree = worktree_manager
            .create(run_id, role.as_str(), commit)
            .await?;
        db::set_cycle_worktree_path(
            &self.pool,
            cycle_id,
            role.as_str(),
            &worktree.path().display().to_string(),
        )
        .await?;

        // ADR-0012: this step's own role, target commit, this cycle's
        // diff, and the findings that triggered the cycle -- plus, since
        // ADR-0013, its own definition's system prompt.
        let stdin_payload = warden_core::build_finding_agent_input_json(
            role.as_str(),
            &agent.system_prompt,
            commit,
            diff,
            prior_findings.to_vec(),
            scope,
        )?;

        let outcome = self
            .run_agent(
                cycle_id,
                role,
                false,
                runner,
                &agent.command,
                env_allowlist,
                worktree.path(),
                &config.repo_path,
                &config.warden_home.join("worktrees").join(run_id),
                &agent.trusted_arg_values,
                stdin_payload,
                cancel.clone(),
            )
            .await?;

        // Issue #71 review (HIGH): a gated step that exited non-zero must
        // never have its stdout trusted at all -- checked *before*
        // `extract_findings` is ever called, independent of whatever that
        // adapter's own mapping does with a blank/malformed buffer.
        // Mirrors the producer path's own non-zero-exit check above (M2),
        // but a blocking finding rather than failing the whole run: a
        // crashed/misbehaving step is exactly the kind of problem a
        // reboucle back to the producer can plausibly recover from (a
        // transient invocation failure, a flaky sandbox, ...), unlike a
        // producer that never produced a commit worth reviewing at all.
        let findings = if outcome.exit_code != 0 {
            tracing::warn!(
                run_id,
                cycle_id,
                role = role.as_str(),
                exit_code = outcome.exit_code,
                stderr = %outcome.stderr,
                "gated step exited non-zero; not trusting its stdout"
            );
            vec![Finding {
                source: warden_core::FindingSource::role(role.as_str()),
                severity: warden_core::Severity::Blocking,
                file: None,
                description: format!(
                    "{role} exited with status {} instead of 0 (stderr: {})",
                    outcome.exit_code,
                    truncate_for_error(&outcome.stderr)
                ),
                action: Some(
                    "investigate why the agent process exited non-zero and fix it".to_string(),
                ),
            }]
        } else {
            // Agent stdout is untrusted input: a parse failure becomes a
            // blocking finding describing the problem, never a run-ending
            // panic (code-standards.md: "Ne jamais faire confiance à la
            // sortie d'un agent CLI"). `runner.extract_findings` is this
            // run's `--tool` adapter's own translation from its CLI's raw
            // output into findings NDJSON (issue #24 point 1, third
            // bullet).
            //
            // Issue #24 review, cycle 2, MAJOR 2: a shape-valid batch isn't
            // necessarily an *honest* one -- `extract_findings` only checks
            // that every finding's `source` is some known value, not that
            // it's the one this role is entitled to claim.
            // `validate_finding_sources_for_role` closes that gap (a forged
            // `source: "warden"`, or a step mislabelling its own failure as
            // a sibling step's own source to slip past `step_succeeded`
            // below) with the exact same "reject the whole batch, describe
            // why, never silently drop/relabel" treatment as an
            // unparsable-output failure -- see that function's own docs
            // for the full rationale.
            match runner
                .extract_findings(&outcome.stdout)
                .and_then(|findings| {
                    warden_core::validate_finding_sources_for_role(&findings, role)?;
                    Ok(findings)
                }) {
                Ok(findings) => findings,
                Err(parse_error) => {
                    tracing::warn!(%parse_error, role = role.as_str(), stdout = %outcome.stdout, "gated step produced unparsable or misattributed output");
                    vec![Finding {
                        source: warden_core::FindingSource::role(role.as_str()),
                        severity: warden_core::Severity::Blocking,
                        file: None,
                        description: format!(
                            "{role} produced unparsable or misattributed output: {parse_error}"
                        ),
                        action: Some("fix the agent's output format/finding sources".to_string()),
                    }]
                }
            }
        };

        // ADR-0009 (issue #7): capture evidence right after a *successful*
        // run of this step, still inside its worktree -- which is about to
        // be removed below, so this must happen before that, not after.
        // Gated on the step's own declared `captures_evidence` (issue #73
        // review, finding F2), never on a literal role name.
        if captures_evidence && step_succeeded(role, &findings) {
            self.capture_evidence_for_cycle(EvidenceCapture {
                run_id,
                cycle_id,
                cycle_number,
                config,
                tester_command: &agent.command,
                tester_worktree_path: worktree.path(),
                cancel,
            })
            .await;
        }

        if let Err(error) = worktree.remove().await {
            tracing::warn!(%error, role = role.as_str(), "failed to clean up worktree after cycle");
        }

        Ok(findings)
    }

    /// The `type: hook` half of [`Orchestrator::run_gated_step`]'s dispatch
    /// (issue #79): no LLM -- runs `invocation.run`'s shell command
    /// deterministically inside a worktree checked out at
    /// `invocation.commit`, the same isolation and bookkeeping
    /// (`db::set_cycle_worktree_path`) any other gated step gets.
    ///
    /// A clean run (policy-allowed, zero exit) returns no findings at all;
    /// a blocked/failing one returns exactly one blocking
    /// [`warden_core::Finding`] sourced as `FindingSource::role(role)` --
    /// the same shape [`Orchestrator::run_gated_agent_step`] already
    /// produces for a crashed agent, so the convergence loop's own gating
    /// (`decide_next_state_for_step`) treats a failing hook step identically
    /// to a failing agent step, with no change needed there at all. The
    /// sandboxed execution itself (`agent_processes` bookkeeping,
    /// cancellation, start/finish events) is
    /// [`Orchestrator::run_step_command`]'s job -- see its own docs.
    ///
    /// Ignores `scope`/`diff`/`prior_findings`/`captures_evidence`: a
    /// deterministic command takes no input payload and
    /// [`warden_core::Workflow::parse_yaml`] already rejects `evidence: true`
    /// on a `type: hook` step, so `captures_evidence` is always `false` here.
    async fn run_gated_hook_step(
        &self,
        invocation: GatedStepInvocation<'_>,
    ) -> Result<Vec<Finding>> {
        let GatedStepInvocation {
            run_id,
            cycle_id,
            role,
            worktree_manager,
            commit,
            run,
            cancel,
            ..
        } = invocation;
        let command = run.expect(
            "run_gated_hook_step is only reached for StepKind::Hook, which always carries \
             \"run\" -- Workflow::parse_yaml's own invariant",
        );

        let worktree = worktree_manager
            .create(run_id, role.as_str(), commit)
            .await?;
        db::set_cycle_worktree_path(
            &self.pool,
            cycle_id,
            role.as_str(),
            &worktree.path().display().to_string(),
        )
        .await?;

        let findings = self
            .run_step_command(run_id, cycle_id, role, worktree.path(), command, cancel)
            .await?;

        if let Err(error) = worktree.remove().await {
            tracing::warn!(%error, role = role.as_str(), "failed to clean up worktree after cycle");
        }

        Ok(findings)
    }

    /// The sandboxed execution of one `type: hook` step's command (issue
    /// #79 review, MEDIUM), split out of
    /// [`Orchestrator::run_gated_hook_step`] purely for readability. Mirrors
    /// [`Orchestrator::run_agent`]'s own shape rather than routing through a
    /// shared free function: a hook step needs the identical sandbox
    /// lifecycle ([`SandboxGuard`]), `agent_processes` bookkeeping,
    /// cancellation, and start/finish event pair every agent invocation
    /// already gets, which lives here, in the orchestrator layer that owns
    /// `self.pool`/`self.publish_event` -- `crate::hook` stays scoped to
    /// lifecycle hooks plus the two small, reusable pieces this borrows
    /// (`evaluate_shell_policy`, the policy check; `trailing_chars`/
    /// `format_tail_suffix`, tail-trimming for the failure reason).
    ///
    /// Policy-evaluated first (issue #51, ADR-0016) -- a denied command
    /// never reaches the sandbox at all, so no `agent_processes` row is
    /// recorded for it (there is no process to track). Otherwise a
    /// [`SandboxGuard`] owns the sandbox from `create` onward (issue #79
    /// review, cycle 3: a hand-paired `create`/`destroy`, this function's
    /// own shape before this fix, missed every early return between the two
    /// -- `db::insert_agent_process`'s and `self.publish_event`'s own `?`
    /// included -- and had no answer at all for this whole future being
    /// dropped mid-`.await`, e.g. a `warden run --tui` exit; under
    /// `--isolation docker`, `destroy` is `docker rm -f`, so either gap
    /// leaked a running container). The guard's own `Drop` is the backstop
    /// for that last case; every other exit goes through the single,
    /// explicit, awaited `guard.destroy()` at the end, whose own failure is
    /// logged (`tracing::warn!`), never silently swallowed.
    ///
    /// Records the `agent_processes` row and publishes
    /// [`RunEvent::AgentStarted`] right after the sandbox reports a pid --
    /// *before* awaiting completion, exactly like [`Orchestrator::run_agent`]
    /// does for an agent subprocess -- so
    /// [`crate::orchestrator::recover_crashed_runs`] can detect and reap a
    /// hook step's orphaned process on restart instead of leaving it
    /// invisible. Marks the row ended and publishes
    /// [`RunEvent::AgentFinished`] (`usage: None` -- a deterministic command
    /// reports no LLM token usage) once the command completes, whichever
    /// way.
    ///
    /// Full stdout/stderr are logged at `tracing::debug!` regardless of
    /// outcome (mirrors [`Orchestrator::run_agent`]'s own L1 stderr
    /// logging) -- otherwise the only durable record of a hook step's
    /// output, since it is never itself an agent whose stdout a
    /// `ToolAdapter` parses. On a non-zero exit, the returned finding's
    /// description carries a trimmed tail of **whichever of stdout/stderr
    /// are non-empty** (both, labeled, when both are -- issue #79 review,
    /// cycle 3: a "stderr, else stdout" heuristic silently dropped stdout
    /// diagnostics the moment a tool emitted *any* stderr noise, e.g. a
    /// cargo manifest warning next to `rustfmt --check`'s own stdout diff);
    /// most linters/formatters -- `rustfmt --check`, `eslint`, most test
    /// runners -- write their diagnostics to stdout, not stderr, so a
    /// reason that only ever carried the exit code was not actionable.
    async fn run_step_command(
        &self,
        run_id: &str,
        cycle_id: &str,
        role: &Role,
        cwd: &Path,
        command: &str,
        cancel: CancellationToken,
    ) -> Result<Vec<Finding>> {
        if let PolicyOutcome::Blocked { reason } =
            crate::hook::evaluate_shell_policy(&self.policy_gate, run_id, command).await
        {
            return Ok(vec![blocking_step_finding(role, reason)]);
        }

        let sandbox_id = self
            .sandbox
            .create(warden_sandbox::SandboxSpec {
                cwd: cwd.to_path_buf(),
            })
            .await?;

        // Issue #79 review, cycle 3: structural create->destroy pairing via
        // `SandboxGuard`, the exact seam `run_agent` already uses -- see
        // that struct's own docs. Everything that can exit early via `?` is
        // inside the block `result` captures, so `guard.destroy()` always
        // runs, awaited, right after, whichever way the block ended.
        let mut guard = SandboxGuard::new(Arc::clone(&self.sandbox), sandbox_id);

        let result: Result<Vec<Finding>> = async {
            let execution = self
                .sandbox
                .execute(
                    guard.id(),
                    warden_sandbox::Command {
                        program: "sh".to_string(),
                        args: vec!["-c".to_string(), command.to_string()],
                        env_allowlist: WORKFLOW_STEP_ENV_ALLOWLIST
                            .iter()
                            .map(|name| name.to_string())
                            .collect(),
                        stdin: None,
                    },
                    warden_sandbox::ExecuteOptions {
                        cancel,
                        on_stdout_line: None,
                    },
                )
                .await?;

            // H1 (mirrors `run_agent`'s own invariant): never persist pid 0
            // or skip recording -- a missing pid right after the sandbox
            // started the process is a typed error, not a silent fallback
            // that would leave this step's process invisible to crash
            // recovery.
            let pid = execution.pid.ok_or_else(|| ProcessError::MissingPid {
                command: command.to_string(),
            })?;

            let process_id = Uuid::new_v4().to_string();
            db::insert_agent_process(
                &self.pool,
                &process_id,
                cycle_id,
                role.as_str(),
                pid,
                &cwd.display().to_string(),
            )
            .await?;
            self.publish_event(RunEvent::AgentStarted {
                role: role.as_str().to_string(),
            })
            .await?;

            let waited = execution.wait().await;
            let exit_code_for_db = match &waited {
                Ok(output) => output.exit_code,
                Err(_) => -1,
            };
            db::mark_agent_process_ended(&self.pool, &process_id, exit_code_for_db).await?;
            let output = waited?;

            tracing::debug!(
                cycle_id,
                role = role.as_str(),
                command,
                exit_code = output.exit_code,
                stdout = %output.stdout,
                stderr = %output.stderr,
                "workflow step command output"
            );

            self.publish_event(RunEvent::AgentFinished {
                role: role.as_str().to_string(),
                exit_code: output.exit_code,
                usage: None,
            })
            .await?;

            if output.exit_code == 0 {
                return Ok(Vec::new());
            }

            let stderr_tail = crate::hook::trailing_chars(&output.stderr, 500);
            let stdout_tail = crate::hook::trailing_chars(&output.stdout, 500);
            let mut tail_parts = Vec::new();
            if !stderr_tail.is_empty() {
                tail_parts.push(format!("stderr: {stderr_tail}"));
            }
            if !stdout_tail.is_empty() {
                tail_parts.push(format!("stdout: {stdout_tail}"));
            }
            let reason = format!(
                "step command `{command}` exited {}{}",
                output.exit_code,
                crate::hook::format_tail_suffix(&tail_parts.join(" | "))
            );
            Ok(vec![blocking_step_finding(role, reason)])
        }
        .await;

        if let Err(error) = guard.destroy().await {
            tracing::warn!(
                cycle_id,
                role = role.as_str(),
                %error,
                "failed to destroy sandbox after workflow step command"
            );
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::test_support::*;
    use std::process::Command as SyncCommand;
    use tempfile::TempDir;

    fn reviewer_role() -> Role {
        Role::new("reviewer").unwrap()
    }

    fn tester_role() -> Role {
        Role::new("tester").unwrap()
    }

    /// Acceptance criterion 1 (issue #2), updated for issue #40's
    /// independent reviewer/tester invocations (the removed
    /// `run_review_and_test` used to exercise this concurrently via
    /// `tokio::join!`; reviewer and tester now run sequentially, see
    /// `run_review_and_test_runs_...` below), further generalized by issue
    /// #73's trio-unification follow-up (both now go through the single
    /// generic [`Orchestrator::run_gated_step`]): reviewer and tester each
    /// write to a DIFFERENT file in their own worktree, then read back the
    /// *other* role's target file from their own worktree. Each gets a
    /// fresh worktree checked out from the same base `commit`
    /// (`WorktreeManager::create`, keyed by role), so if the two ever
    /// shared a worktree/directory, the other role's write would already be
    /// visible here instead of the original, untouched content -- regardless
    /// of whether the two run concurrently or in sequence, this is what
    /// distinguishes "isolated worktrees" from "shared worktree".
    #[tokio::test]
    async fn run_review_and_test_isolates_writes_to_different_worktree_files() {
        let repo = init_test_repo();
        std::fs::write(repo.path().join("review_target.txt"), "original-review\n").unwrap();
        std::fs::write(repo.path().join("test_target.txt"), "original-test\n").unwrap();
        let commit = |args: &[&str]| {
            let status = SyncCommand::new("git")
                .current_dir(repo.path())
                .args(args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        };
        commit(&["add", "."]);
        commit(&["commit", "--quiet", "-m", "add review/test targets"]);

        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();

        db::insert_run(
            &pool,
            "collision-run",
            &repo.path().display().to_string(),
            "main",
            "crossed findings, no collision",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "collision-cycle", "collision-run", 1)
            .await
            .unwrap();

        let reviewer_command = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"
                    echo modified-by-reviewer > review_target.txt
                    seen=$(cat test_target.txt)
                    echo "{\"source\":\"reviewer\",\"severity\":\"info\",\"description\":\"review_target=modified-by-reviewer test_target_seen=$seen\"}"
                    "#,
            ],
        );
        let tester_command = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"
                    echo modified-by-tester > test_target.txt
                    seen=$(cat review_target.txt)
                    echo "{\"source\":\"tester\",\"severity\":\"info\",\"description\":\"test_target=modified-by-tester review_target_seen=$seen\"}"
                    "#,
            ],
        );
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "crossed findings, no collision".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(reviewer_command),
                definition(tester_command),
            ],
            evidence_tool: None,
            evidence_store_in_repo: true,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();

        let orchestrator = Orchestrator::new(pool.clone());
        let reviewer_role = reviewer_role();
        let mut findings = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "collision-run",
                    cycle_id: "collision-cycle",
                    cycle_number: 1,
                    step_index: 1,
                    role: &reviewer_role,
                    agent: agents.steps[1].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: false,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        let tester_role = tester_role();
        findings.extend(
            orchestrator
                .run_gated_step(
                    &FakeCommandAdapter,
                    GatedStepInvocation {
                        run_id: "collision-run",
                        cycle_id: "collision-cycle",
                        cycle_number: 1,
                        step_index: 2,
                        role: &tester_role,
                        agent: agents.steps[2].as_ref(),
                        kind: warden_core::StepKind::Agent,
                        run: None,
                        env_allowlist: agents.env_allowlist,
                        worktree_manager: &worktree_manager,
                        commit: "HEAD",
                        diff: "",
                        prior_findings: &[],
                        scope: warden_core::ReviewScope::Full,
                        captures_evidence: true,
                        config: &config,
                        cancel: CancellationToken::new(),
                    },
                )
                .await
                .unwrap(),
        );

        assert_eq!(findings.len(), 2);
        let reviewer_finding = findings
            .iter()
            .find(|f| f.source == warden_core::FindingSource::role("reviewer"))
            .expect("reviewer finding present");
        let tester_finding = findings
            .iter()
            .find(|f| f.source == warden_core::FindingSource::role("tester"))
            .expect("tester finding present");

        assert!(
            reviewer_finding
                .description
                .contains("test_target_seen=original-test"),
            "reviewer's worktree must still see the untouched original \
                 test_target.txt, not the tester's write -- got: {}",
            reviewer_finding.description
        );
        assert!(
            tester_finding
                .description
                .contains("review_target_seen=original-review"),
            "tester's worktree must still see the untouched original \
                 review_target.txt, not the reviewer's write -- got: {}",
            tester_finding.description
        );
    }

    /// Issue #40 / decision #37 Q2, generalized by issue #73's trio-
    /// unification follow-up: a step invoked at `step_index: 1` with
    /// `ReviewScope::Correctif` must receive a payload scoped to the
    /// correctif's own diff plus the findings that prompted it -- captured
    /// directly from what the agent actually reads off stdin.
    #[tokio::test]
    async fn a_step_1_invocation_with_a_correctif_scope_sends_a_scoped_payload() {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;
        let payloads = TempDir::new().unwrap();

        db::insert_run(
            &pool,
            "scoped-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "scoped-cycle", "scoped-run", 1)
            .await
            .unwrap();

        let capturing_reviewer = AgentCommand::new(
            "sh",
            [
                "-c",
                &format!("cat > '{}/reviewer.json'", payloads.path().display()),
            ],
        );
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(capturing_reviewer),
                definition(always_passing_tester()),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let originating_finding = Finding {
            source: warden_core::FindingSource::role("reviewer"),
            severity: warden_core::Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "unchecked unwrap".to_string(),
            action: Some("handle the error".to_string()),
        };

        let reviewer_role = reviewer_role();
        orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "scoped-run",
                    cycle_id: "scoped-cycle",
                    cycle_number: 1,
                    step_index: 1,
                    role: &reviewer_role,
                    agent: agents.steps[1].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "diff --git a/x b/x\n+fixed the unwrap\n",
                    prior_findings: std::slice::from_ref(&originating_finding),
                    scope: warden_core::ReviewScope::Correctif,
                    captures_evidence: false,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        let raw = std::fs::read_to_string(payloads.path().join("reviewer.json"))
            .expect("reviewer payload must have been captured");
        let payload = warden_core::parse_agent_input_message(&raw)
            .expect("a payload warden's own parser accepts");

        assert_eq!(payload.scope, warden_core::ReviewScope::Correctif);
        assert_eq!(
            payload.diff.as_deref(),
            Some("diff --git a/x b/x\n+fixed the unwrap\n")
        );
        assert_eq!(payload.findings, vec![originating_finding]);
    }

    /// Issue #40, generalized by issue #73's trio-unification follow-up:
    /// `run_gated_step` must refuse a `Correctif` scope for any step but
    /// `step_index == 1` -- defense in depth against a future caller
    /// constructing a `GatedStepInvocation` directly with a mismatched
    /// index/scope pair.
    #[tokio::test]
    async fn run_gated_step_rejects_a_correctif_scope_for_a_non_first_gated_step() {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;

        db::insert_run(
            &pool,
            "bad-scope-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "bad-scope-cycle", "bad-scope-run", 1)
            .await
            .unwrap();

        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(always_passing_tester()),
                definition(always_passing_tester()),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let tester_role = tester_role();
        let result = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "bad-scope-run",
                    cycle_id: "bad-scope-cycle",
                    cycle_number: 1,
                    step_index: 2,
                    role: &tester_role,
                    agent: agents.steps[2].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Correctif,
                    captures_evidence: true,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await;

        assert!(
            matches!(
                result,
                Err(WardenError::Core(
                    warden_core::CoreError::MalformedAgentInput(_)
                ))
            ),
            "expected a typed rejection, got: {result:?}"
        );
    }

    /// Issue #40 (ADR-0003 amendment), generalized by issue #73's trio-
    /// unification follow-up: reviewer and tester must now run
    /// **sequentially** -- the opposite of what this test asserted before
    /// the removed `run_review_and_test`'s `tokio::join!` path. Regression
    /// coverage for "no one quietly reintroduces `tokio::join!`/`try_join!`
    /// here": one `run_gated_step` call immediately followed by another,
    /// each backed by a sleepy agent, must together take at least as long as
    /// both sleeps combined, not just the slower one.
    ///
    /// Deliberately not a fixed wall-clock threshold (e.g. `elapsed >
    /// 1.9 * SLEEP`): under cargo's default parallel test harness, `git
    /// worktree add` contention and process-spawn overhead from other
    /// worktree-creating tests running at the same time can push a single
    /// absolute bound past its margin without anything actually being wrong
    /// -- non-deterministic per code-standards.md line 17. Instead this
    /// asserts on a *ratio* against `SLEEP` alone: a concurrent
    /// (`tokio::join!`) path would land close to 1x `SLEEP` plus overhead; a
    /// sequential one lands close to 2x. 1.5x is comfortably above the
    /// concurrent case and comfortably below the sequential one regardless
    /// of ambient load.
    #[tokio::test]
    async fn run_review_and_test_runs_reviewer_and_tester_sequentially_not_concurrently() {
        const SLEEP: Duration = Duration::from_millis(500);

        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();

        let sleepy_agent = AgentCommand::new("sh", ["-c", "sleep 0.5"]);
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "timing check".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(sleepy_agent.clone()),
                definition(sleepy_agent),
            ],
            evidence_tool: None,
            evidence_store_in_repo: true,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };

        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();

        let orchestrator = Orchestrator::new(pool.clone());

        db::insert_run(
            &pool,
            "timing-run",
            &repo.path().display().to_string(),
            "main",
            "timing check",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "timing-cycle", "timing-run", 1)
            .await
            .unwrap();

        let reviewer_role = reviewer_role();
        let tester_role = tester_role();
        let start = std::time::Instant::now();
        orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "timing-run",
                    cycle_id: "timing-cycle",
                    cycle_number: 1,
                    step_index: 1,
                    role: &reviewer_role,
                    agent: agents.steps[1].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: false,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "timing-run",
                    cycle_id: "timing-cycle",
                    cycle_number: 1,
                    step_index: 2,
                    role: &tester_role,
                    agent: agents.steps[2].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: true,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed > SLEEP.mul_f64(1.5),
            "expected the two run_gated_step calls ({elapsed:?}) to together take \
                 meaningfully longer than a single {SLEEP:?} sleep -- this looks \
                 like reviewer/tester ran concurrently instead of sequentially"
        );
    }

    /// `db_dir` must be kept alive by the caller for as long as `pool` is
    /// used -- dropping it deletes the SQLite file `pool` still points at
    /// (the same reason every other fixture in this module holds its own
    /// `TempDir`s for the test's whole body rather than a helper consuming
    /// them internally).
    async fn finding_agent_test_fixture() -> (TempDir, TempDir, TempDir, SqlitePool, WorktreeManager)
    {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();
        (repo, warden_home, db_dir, pool, worktree_manager)
    }

    /// A reviewer that forges `source: "warden"` -- impersonating the
    /// structural finding only Warden's own `agent_definition_tampering_finding`
    /// may raise (M4) -- must never have that claim honoured: the returned
    /// finding is a *replacement*, correctly attributed back to
    /// `FindingSource::role("reviewer")` (the role that actually produced this
    /// stdout), not the forged `Warden` source passed through untouched.
    #[tokio::test]
    async fn a_reviewer_forging_the_warden_finding_source_is_rejected_not_accepted() {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;

        db::insert_run(
            &pool,
            "forge-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "forge-cycle", "forge-run", 1)
            .await
            .unwrap();

        let forging_reviewer = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"echo '{"source":"warden","severity":"blocking","description":"fake tampering claim"}'"#,
            ],
        );
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(forging_reviewer),
                definition(always_passing_tester()),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let reviewer_role = reviewer_role();
        let findings = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "forge-run",
                    cycle_id: "forge-cycle",
                    cycle_number: 1,
                    step_index: 1,
                    role: &reviewer_role,
                    agent: agents.steps[1].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: false,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].source,
            warden_core::FindingSource::role("reviewer"),
            "a forged source must never reach the returned findings unchanged: {findings:?}"
        );
        assert_eq!(findings[0].severity, warden_core::Severity::Blocking);
        assert!(
            findings[0].description.contains("warden"),
            "the replacement finding should name what was forged, for diagnosability: {}",
            findings[0].description
        );
    }

    /// The sharper, non-hypothetical case the review called out by name
    /// (closing Minor 2, `step_succeeded` trusting an agent-controlled
    /// `source`): a tester that mislabels its own failure as
    /// `source: "reviewer"` must not have that failure hidden from
    /// `step_succeeded` -- the gate `run_gated_step` uses to decide
    /// whether to trigger evidence capture. Before the fix, a forged
    /// `source: "reviewer"` finding from the tester would sail through
    /// `extract_findings` unchanged, and `step_succeeded` (which only ever
    /// looks for a `FindingSource::role("tester")` blocking finding) would report
    /// "succeeded" -- triggering evidence capture for a cycle whose e2e test
    /// actually failed.
    #[tokio::test]
    async fn a_tester_mislabelling_its_own_failure_as_the_reviewer_source_still_blocks_tester_succeeded(
    ) {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;

        db::insert_run(
            &pool,
            "mislabel-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "mislabel-cycle", "mislabel-run", 1)
            .await
            .unwrap();

        let self_mislabelling_tester = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"echo '{"source":"reviewer","severity":"blocking","description":"secretly failing"}'"#,
            ],
        );
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(always_passing_tester()),
                definition(self_mislabelling_tester),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let tester_role = tester_role();
        let findings = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "mislabel-run",
                    cycle_id: "mislabel-cycle",
                    cycle_number: 1,
                    step_index: 2,
                    role: &tester_role,
                    agent: agents.steps[2].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: true,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].source,
            warden_core::FindingSource::role("tester"),
            "the tester's own mislabelled finding must be re-attributed to Tester, not left as \
                 the forged Reviewer source: {findings:?}"
        );
        assert_eq!(findings[0].severity, warden_core::Severity::Blocking);
        assert!(
            !step_succeeded(&tester_role, &findings),
            "Minor 2: a tester that mislabels its own failure must still be seen as failed by \
                 step_succeeded, the gate that decides whether to trigger evidence capture"
        );
    }

    /// Issue #71 review (HIGH), tester side: the exit-code guard added to
    /// `run_gated_step` closes the fail-open "for every `--tool` adapter at
    /// once, not just mistral" (see this fix's own commit message) -- this
    /// repo's only other regression coverage for that guard
    /// (`e2e_mistral_reviewer_exiting_nonzero_with_no_output_never_converges`,
    /// `crates/warden/tests/cli.rs`) drives it exclusively through the
    /// **reviewer** role. Since every gated step shares the exact same
    /// `run_gated_step` body now, the guard is structurally the same code
    /// path for all of them -- but that's an implementation detail a future
    /// refactor could break without any test failing today. This exercises
    /// the **tester** side directly, with a tester that exits non-zero and
    /// prints nothing to stdout: it must come back as a synthesized blocking
    /// `Tester` finding naming the exit status, never as "zero findings"
    /// (which `step_succeeded` would otherwise read as a passing test
    /// suite and wrongly trigger evidence capture for).
    #[tokio::test]
    async fn a_tester_that_exits_nonzero_with_no_output_synthesizes_a_blocking_finding_not_a_silent_pass(
    ) {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;

        db::insert_run(
            &pool,
            "tester-crash-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "tester-crash-cycle", "tester-crash-run", 1)
            .await
            .unwrap();

        let crashing_tester = AgentCommand::new("sh", ["-c", "printf 'boom' >&2; exit 7"]);
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(always_passing_tester()),
                definition(crashing_tester),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let tester_role = tester_role();
        let findings = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "tester-crash-run",
                    cycle_id: "tester-crash-cycle",
                    cycle_number: 1,
                    step_index: 2,
                    role: &tester_role,
                    agent: agents.steps[2].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: true,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            findings.len(),
            1,
            "a crashing tester must synthesize exactly one blocking finding, not be silently \
                 read as zero findings: {findings:?}"
        );
        assert_eq!(
            findings[0].source,
            warden_core::FindingSource::role("tester")
        );
        assert_eq!(findings[0].severity, warden_core::Severity::Blocking);
        assert!(
            findings[0].description.contains("exited with status 7"),
            "the synthesized finding should name the actual exit status: {}",
            findings[0].description
        );
        assert!(
            !step_succeeded(&tester_role, &findings),
            "a tester that crashed non-zero must never be read as a passing test suite by \
                 step_succeeded, the gate that decides whether to trigger evidence capture"
        );
    }

    /// The legitimate control: a reviewer emitting its own, correct source
    /// must pass through completely unchanged -- proving the validation
    /// added above rejects only a genuine mismatch, not every finding.
    #[tokio::test]
    async fn a_reviewer_finding_with_its_own_correct_source_passes_through_unchanged() {
        let (repo, warden_home, _db_dir, pool, worktree_manager) =
            finding_agent_test_fixture().await;

        db::insert_run(
            &pool,
            "legit-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "legit-cycle", "legit-run", 1)
            .await
            .unwrap();

        let honest_reviewer = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"echo '{"source":"reviewer","severity":"warning","description":"looks mostly fine","file":"src/lib.rs"}'"#,
            ],
        );
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow: warden_core::Workflow::builtin_default(),
            max_extra_step_cycles: 5,
            step_agents: vec![
                definition(AgentCommand::new("sh", ["-c", "true"])),
                definition(honest_reviewer),
                definition(always_passing_tester()),
            ],
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let reviewer_role = reviewer_role();
        let findings = orchestrator
            .run_gated_step(
                &FakeCommandAdapter,
                GatedStepInvocation {
                    run_id: "legit-run",
                    cycle_id: "legit-cycle",
                    cycle_number: 1,
                    step_index: 1,
                    role: &reviewer_role,
                    agent: agents.steps[1].as_ref(),
                    kind: warden_core::StepKind::Agent,
                    run: None,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    captures_evidence: false,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            findings,
            vec![Finding {
                source: warden_core::FindingSource::role("reviewer"),
                severity: warden_core::Severity::Warning,
                file: Some("src/lib.rs".to_string()),
                description: "looks mostly fine".to_string(),
                action: None,
            }]
        );
    }

    // -----------------------------------------------------------------
    // Issue #79 review: `run_step_command` -- a `type: hook` step's own
    // sandboxed execution (cancellation, env allowlist).
    // -----------------------------------------------------------------

    async fn orchestrator_with_run_and_cycle(
        pool: &SqlitePool,
        run_id: &str,
        cycle_id: &str,
    ) -> Orchestrator {
        db::insert_run(pool, run_id, "/tmp/repo", "main", "intent", 3, 3, 3, 5)
            .await
            .unwrap();
        db::insert_cycle(pool, cycle_id, run_id, 1).await.unwrap();
        Orchestrator::new(pool.clone())
    }

    /// Issue #79 review, MEDIUM: the run's own `CancellationToken` must
    /// actually reach a `type: hook` step's sandboxed command -- before this
    /// fix it was silently dropped (`ExecuteOptions::default()`), so
    /// Ctrl-C/TUI-exit never stopped a long-running hook command. Mirrors
    /// `agent_run`'s own
    /// `sandbox_is_destroyed_when_cancellation_resolves_the_future_normally`.
    #[tokio::test]
    async fn a_hook_steps_command_is_cancelled_when_the_run_cancellation_token_fires() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let orchestrator =
            orchestrator_with_run_and_cycle(&pool, "cancel-hook-run", "cancel-hook-cycle").await;
        let cwd = TempDir::new().unwrap();
        let role = Role::new("lint").unwrap();

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        let result = orchestrator
            .run_step_command(
                "cancel-hook-run",
                "cancel-hook-cycle",
                &role,
                cwd.path(),
                "sleep 30",
                cancel,
            )
            .await;

        assert!(
            matches!(
                result,
                Err(WardenError::Sandbox(
                    warden_sandbox::SandboxError::Cancelled { .. }
                ))
            ),
            "expected the hook step's command to be cancelled, got {result:?}"
        );
    }

    /// Issue #79 review, HIGH: a `type: hook` step's command must forward
    /// only [`WORKFLOW_STEP_ENV_ALLOWLIST`], never the operator's full
    /// environment (`crate::hook::CommandHook::full_env_allowlist`).
    /// `CARGO_MANIFEST_DIR` -- reliably set by `cargo test` in this
    /// process's own environment, read-only here -- stands in for "a
    /// variable not on that list", the same technique
    /// `warden_sandbox::local`'s own `env_clear_means_an_unallowlisted_variable_never_reaches_the_child`/
    /// `env_allowlist_forwards_only_the_named_variables` tests use, rather
    /// than `std::env::set_var` (`unsafe`, and cross-test interference risk
    /// under a parallel test runner -- see those tests' own docs).
    #[tokio::test]
    async fn a_hook_steps_command_only_forwards_the_narrow_workflow_step_allowlist() {
        assert!(
            std::env::var("CARGO_MANIFEST_DIR").is_ok(),
            "precondition: cargo test sets CARGO_MANIFEST_DIR"
        );

        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let orchestrator =
            orchestrator_with_run_and_cycle(&pool, "env-hook-run", "env-hook-cycle").await;
        let cwd = TempDir::new().unwrap();
        let role = Role::new("lint").unwrap();

        let findings = orchestrator
            .run_step_command(
                "env-hook-run",
                "env-hook-cycle",
                &role,
                cwd.path(),
                r#"if [ -n "$CARGO_MANIFEST_DIR" ]; then echo leaked; exit 1; fi"#,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(
            findings.is_empty(),
            "CARGO_MANIFEST_DIR is not on WORKFLOW_STEP_ENV_ALLOWLIST and must never reach the \
                 command's own environment: {findings:?}"
        );
    }

    /// Issue #79 review, cycle 3: a failing command that writes to *both*
    /// stdout and stderr must have both tails in its finding's description
    /// -- before this fix, any non-empty stderr (a cargo manifest warning, a
    /// deprecation notice) silently discarded stdout entirely, even though
    /// most linters/formatters (`rustfmt --check`) put their actual
    /// diagnostic there.
    #[tokio::test]
    async fn a_failing_command_with_both_stdout_and_stderr_includes_both_tails() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let orchestrator =
            orchestrator_with_run_and_cycle(&pool, "both-tails-run", "both-tails-cycle").await;
        let cwd = TempDir::new().unwrap();
        let role = Role::new("lint").unwrap();

        let findings = orchestrator
            .run_step_command(
                "both-tails-run",
                "both-tails-cycle",
                &role,
                cwd.path(),
                "echo out-marker; echo err-marker >&2; exit 1",
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].description.contains("out-marker")
                && findings[0].description.contains("err-marker"),
            "expected both the stdout and stderr tails in the description: {}",
            findings[0].description
        );
    }

    /// Issue #79 review, cycle 3: pins the exact allowlist -- without this,
    /// a future edit (e.g. adding `GH_TOKEN`) would silently widen a `type:
    /// hook` step's own trust boundary with no test failing.
    #[test]
    fn workflow_step_env_allowlist_is_pinned() {
        assert_eq!(
            WORKFLOW_STEP_ENV_ALLOWLIST,
            &["HOME", "LANG", "TERM", "CARGO_HOME", "RUSTUP_HOME"]
        );
    }

    /// Issue #79 review, cycle 3: the sandbox created for a `type: hook`
    /// step's command must always be destroyed -- proven directly against a
    /// `RecordingSandbox` (shared with `agent_run`'s own equivalent
    /// coverage, `sandbox_is_destroyed_when_cancellation_resolves_the_future_normally`),
    /// pinning the `create`/`execute`/`destroy` call order `SandboxGuard`
    /// now guarantees here too, instead of the three hand-paired `destroy`
    /// calls this function used before this fix (which missed the
    /// `db::insert_agent_process`/`publish_event` early returns between
    /// them).
    #[tokio::test]
    async fn a_hook_steps_sandbox_is_destroyed_after_its_command_completes() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        db::insert_run(
            &pool,
            "destroy-hook-run",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "destroy-hook-cycle", "destroy-hook-run", 1)
            .await
            .unwrap();
        let sandbox = Arc::new(RecordingSandbox::new(false));
        let orchestrator =
            Orchestrator::new(pool.clone()).with_sandbox(sandbox.clone() as Arc<dyn Sandbox>);
        let cwd = TempDir::new().unwrap();
        let role = Role::new("lint").unwrap();

        orchestrator
            .run_step_command(
                "destroy-hook-run",
                "destroy-hook-cycle",
                &role,
                cwd.path(),
                "true",
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            sandbox.calls(),
            vec!["create", "execute", "destroy"],
            "the sandbox created for the hook step's command must be destroyed exactly once"
        );
    }

    /// Independently-derived gap (test review of the fix above): the
    /// happy-path `create`/`execute`/`destroy` assertion in
    /// [`a_hook_steps_sandbox_is_destroyed_after_its_command_completes`] is
    /// also satisfied by the pre-fix, hand-paired `create`/`destroy` code
    /// this function used to have -- it never pins the actual property the
    /// `SandboxGuard` migration exists for, which is teardown when
    /// `run_step_command`'s own future is dropped mid-`.await`, never
    /// resolving to completion or to an early `?` return at all (run
    /// cancellation via `warden run --tui` exit, a task abort). Mirrors
    /// `agent_run`'s own
    /// [`sandbox_is_destroyed_when_the_run_agent_future_itself_is_dropped_mid_flight`]:
    /// aborts the task running `run_step_command` while it's parked on a
    /// long `sleep 30`, then polls `sandbox.calls()` for `destroy`, since
    /// `SandboxGuard::drop`'s backstop teardown runs on its own detached
    /// task, not awaited by anything after the abort.
    #[tokio::test]
    async fn a_hook_steps_sandbox_is_destroyed_when_run_step_command_is_dropped_mid_flight() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        db::insert_run(
            &pool,
            "abort-hook-run",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "abort-hook-cycle", "abort-hook-run", 1)
            .await
            .unwrap();
        let sandbox = Arc::new(RecordingSandbox::new(false));
        let orchestrator = Arc::new(
            Orchestrator::new(pool.clone()).with_sandbox(sandbox.clone() as Arc<dyn Sandbox>),
        );
        let orchestrator_for_task = Arc::clone(&orchestrator);
        let cwd = TempDir::new().unwrap();
        let cwd_path = cwd.path().to_path_buf();

        let handle = tokio::spawn(async move {
            let role = Role::new("lint").unwrap();
            let _ = orchestrator_for_task
                .run_step_command(
                    "abort-hook-run",
                    "abort-hook-cycle",
                    &role,
                    &cwd_path,
                    "sleep 30",
                    CancellationToken::new(),
                )
                .await;
        });

        // Best-effort delay to let the task get past `create` and into the
        // long `execute`/`wait()` await before dropping it mid-flight --
        // mirrors `agent_run`'s own equivalent test, and its own LOW E note:
        // this is not a synchronization point, only the property under test
        // (the sandbox created for this invocation is destroyed) is
        // asserted below, not the exact call vector.
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();
        let _ = handle.await;

        for _ in 0..200 {
            if sandbox.calls().contains(&"destroy") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let calls = sandbox.calls();
        assert!(
            calls.contains(&"create"),
            "expected the sandbox to have been created before the abort, got {calls:?}"
        );
        assert!(
            calls.contains(&"destroy"),
            "expected `SandboxGuard::drop`'s backstop to destroy the sandbox created for a \
                 `type: hook` step's command whose future was dropped mid-flight, got {calls:?}"
        );
    }
}
