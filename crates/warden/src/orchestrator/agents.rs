//! Coder/reviewer/tester invocation: [`Orchestrator::run_coder`],
//! [`Orchestrator::run_review`]/[`Orchestrator::run_test`] (independent
//! since issue #40), and the shared [`Orchestrator::run_finding_agent`]
//! underneath the latter two.

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
/// le succès du test e2e"), inferred as "the tester itself raised no
/// blocking finding" -- there's no separate pass/fail signal in the
/// findings protocol, so absence of a blocking `Tester`-sourced finding is
/// the only available proxy.
fn tester_succeeded(findings: &[Finding]) -> bool {
    !findings.iter().any(|finding| {
        finding.source == warden_core::FindingSource::Tester
            && finding.severity == warden_core::Severity::Blocking
    })
}

fn role_to_finding_source(role: AgentRole) -> warden_core::FindingSource {
    match role {
        AgentRole::Reviewer => warden_core::FindingSource::Reviewer,
        AgentRole::Tester => warden_core::FindingSource::Tester,
        // Coder never produces findings; only used defensively.
        AgentRole::Coder => warden_core::FindingSource::Reviewer,
    }
}

impl Orchestrator {
    pub(super) async fn run_coder<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: CoderInvocation<'_>,
    ) -> Result<CoderCycleResult> {
        let CoderInvocation {
            run_id,
            cycle_id,
            cycle_number,
            config,
            agent,
            env_allowlist,
            worktree_manager,
            base_commit,
            run_agent_definition_snapshot,
            prior_findings,
            cancel,
        } = invocation;

        let worktree = worktree_manager
            .create(run_id, AgentRole::Coder.as_str(), base_commit)
            .await?;
        db::set_cycle_worktree_path(
            &self.pool,
            cycle_id,
            AgentRole::Coder,
            &worktree.path().display().to_string(),
        )
        .await?;

        // ADR-0012: resolved right after the worktree is created (before
        // the coder runs), so it's a concrete SHA rather than the possibly
        // ambiguous `base_commit` ref (e.g. the literal string `"HEAD"` on a
        // run's first cycle) -- needed below to compute the diff this
        // cycle's coder introduces, once it has run.
        let base_commit_sha = read_head_commit(worktree.path()).await?;

        // ADR-0013: the coder's own definition (system prompt), the run
        // intent, and -- A2 -- the findings it is being asked to fix. No
        // `target_commit`/`diff`: this very worktree is already checked out
        // at that commit, so the coder can `git diff` for itself rather than
        // be handed a copy of what's on its own disk.
        let stdin_payload = warden_core::AgentInputMessage::for_coder(
            &agent.system_prompt,
            config.intent.clone(),
            prior_findings.to_vec(),
        )?
        .to_json()?;
        let outcome = self
            .run_agent(
                cycle_id,
                AgentRole::Coder,
                runner,
                &agent.command,
                env_allowlist,
                worktree.path(),
                &config.repo_path,
                &config.warden_home.join("worktrees").join(run_id),
                stdin_payload,
                cancel,
            )
            .await?;

        // M2: a coder that exits non-zero has not reliably produced a
        // commit worth reviewing — `read_head_commit` below would just
        // return the unchanged base commit, silently making the loop look
        // like a no-op success. Fail the run explicitly instead.
        if outcome.exit_code != 0 {
            tracing::warn!(
                run_id,
                cycle_id,
                exit_code = outcome.exit_code,
                stderr = %outcome.stderr,
                "coder exited with a non-zero status; failing the run"
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
                tracing::warn!(%error, "failed to clean up coder worktree after a failed coder run");
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
        // object store) -- this is what the reviewer/tester's
        // `AgentInputMessage::diff` carries.
        let diff = read_diff(worktree.path(), &base_commit_sha, &new_commit).await?;

        // Issue #30 (review, HIGH): re-resolves all three roles' raw
        // definition bytes through a throwaway `git worktree` checkout of
        // `new_commit` -- deliberately not this cycle's own coder worktree
        // working directory, which is mutable and not what actually
        // propagates forward -- and compares each against
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
            tracing::warn!(%error, "failed to clean up coder worktree after cycle");
        }

        Ok(CoderCycleResult {
            commit: new_commit,
            diff,
            definition_tampering_finding,
        })
    }

    /// Independent reviewer invocation (issue #40): its own worktree, its
    /// own agent spawn, its own findings extraction -- no longer entangled
    /// with the tester's via `tokio::join!` (ADR-0003 amendment; the removed
    /// `run_review_and_test` used to run both concurrently). Thin
    /// role-fixing wrapper around `run_finding_agent`, which still does the
    /// actual work; kept as its own named entry point so callers -- the
    /// gate-review loop (issue #41) and its Phase B scoped-re-review
    /// follow-up (#42) -- have a `Reviewer`-only seam distinct from
    /// [`Self::run_test`], the one that can be invoked scoped to a single
    /// correctif (`invocation.scope`, decision #37 Q2).
    pub(super) async fn run_review<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: ReviewInvocation<'_>,
    ) -> Result<Vec<Finding>> {
        let ReviewInvocation {
            run_id,
            cycle_id,
            cycle_number,
            agent,
            env_allowlist,
            worktree_manager,
            commit,
            diff,
            prior_findings,
            scope,
            config,
            cancel,
        } = invocation;
        self.run_finding_agent(
            runner,
            FindingAgentInvocation {
                run_id,
                cycle_id,
                cycle_number,
                role: AgentRole::Reviewer,
                agent,
                env_allowlist,
                worktree_manager,
                commit,
                diff,
                prior_findings,
                scope,
                config,
                cancel,
            },
        )
        .await
    }

    /// Independent tester invocation (issue #40): [`Self::run_review`]'s
    /// mirror image, minus the `scope` axis a tester is never invoked with
    /// (decision #37 Q2 only scopes the reviewer).
    pub(super) async fn run_test<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: TestInvocation<'_>,
    ) -> Result<Vec<Finding>> {
        let TestInvocation {
            run_id,
            cycle_id,
            cycle_number,
            agent,
            env_allowlist,
            worktree_manager,
            commit,
            diff,
            prior_findings,
            config,
            cancel,
        } = invocation;
        self.run_finding_agent(
            runner,
            FindingAgentInvocation {
                run_id,
                cycle_id,
                cycle_number,
                role: AgentRole::Tester,
                agent,
                env_allowlist,
                worktree_manager,
                commit,
                diff,
                prior_findings,
                scope: warden_core::ReviewScope::Full,
                config,
                cancel,
            },
        )
        .await
    }

    async fn run_finding_agent<R: ToolAdapter>(
        &self,
        runner: &R,
        invocation: FindingAgentInvocation<'_>,
    ) -> Result<Vec<Finding>> {
        let FindingAgentInvocation {
            run_id,
            cycle_id,
            cycle_number,
            role,
            agent,
            env_allowlist,
            worktree_manager,
            commit,
            diff,
            prior_findings,
            scope,
            config,
            cancel,
        } = invocation;

        let worktree = worktree_manager
            .create(run_id, role.as_str(), commit)
            .await?;
        db::set_cycle_worktree_path(
            &self.pool,
            cycle_id,
            role,
            &worktree.path().display().to_string(),
        )
        .await?;

        // ADR-0012: the reviewer/tester's own role, target commit, this
        // cycle's diff, and the findings that triggered the cycle -- plus,
        // since ADR-0013, its own definition's system prompt. `Correctif`
        // (issue #40) is reviewer-only -- `TestInvocation` carries no
        // `scope` field at all, so `run_test` can never reach that branch,
        // but the match below still refuses it defensively for any other
        // future caller of `run_finding_agent` rather than silently falling
        // back to `Full` (code-standards.md: no silent fallback).
        // `for_finding_agent`/`for_scoped_review` both refuse
        // `AgentRole::Coder`, which can never happen here since `role` is
        // always `Reviewer`/`Tester` at this call site.
        let stdin_payload = match (role, scope) {
            (AgentRole::Reviewer, warden_core::ReviewScope::Correctif) => {
                warden_core::AgentInputMessage::for_scoped_review(
                    &agent.system_prompt,
                    commit,
                    diff,
                    prior_findings.to_vec(),
                )?
            }
            (_, warden_core::ReviewScope::Correctif) => {
                return Err(WardenError::Core(
                    warden_core::CoreError::MalformedAgentInput(format!(
                        "{} cannot be invoked with a scoped (\"correctif\") review -- only the \
                             reviewer can be scoped",
                        role.as_str()
                    )),
                ));
            }
            (_, warden_core::ReviewScope::Full) => {
                warden_core::AgentInputMessage::for_finding_agent(
                    role,
                    &agent.system_prompt,
                    commit,
                    diff,
                    prior_findings.to_vec(),
                )?
            }
        }
        .to_json()?;

        let outcome = self
            .run_agent(
                cycle_id,
                role,
                runner,
                &agent.command,
                env_allowlist,
                worktree.path(),
                &config.repo_path,
                &config.warden_home.join("worktrees").join(run_id),
                stdin_payload,
                cancel.clone(),
            )
            .await?;

        // Issue #71 review (HIGH): a reviewer/tester that exited non-zero
        // must never have its stdout trusted at all -- checked *before*
        // `extract_findings` is ever called, independent of whatever that
        // adapter's own mapping does with a blank/malformed buffer.
        // Mirrors the coder path's own non-zero-exit check above (M2), but
        // a blocking finding rather than failing the whole run: a
        // crashed/misbehaving reviewer or tester is exactly the kind of
        // problem a reboucle back to the coder can plausibly recover from
        // (a transient invocation failure, a flaky sandbox, ...), unlike a
        // coder that never produced a commit worth reviewing at all. This
        // closes a fail-open some adapters could otherwise incidentally
        // reopen: `MistralAdapter::extract_findings` (see its own docs)
        // trusts a blank buffer as "no findings" precisely *because* this
        // check has already confirmed the process exited cleanly --
        // without it, a `mistral` reviewer that crashed and printed
        // nothing would read as a clean, converging pass.
        let findings = if outcome.exit_code != 0 {
            tracing::warn!(
                run_id,
                cycle_id,
                ?role,
                exit_code = outcome.exit_code,
                stderr = %outcome.stderr,
                "agent exited non-zero; not trusting its stdout"
            );
            vec![Finding {
                source: role_to_finding_source(role),
                severity: warden_core::Severity::Blocking,
                file: None,
                description: format!(
                    "{role:?} exited with status {} instead of 0 (stderr: {})",
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
            // bullet) -- the user no longer writes that translation as a
            // wrapper script.
            //
            // Issue #24 review, cycle 2, MAJOR 2: a shape-valid batch isn't
            // necessarily an *honest* one -- `extract_findings` only checks
            // that every finding's `source` is some known value, not that
            // it's the one this role is entitled to claim.
            // `validate_finding_sources_for_role` closes that gap (a forged
            // `source: "warden"`, or a tester mislabelling its own failure
            // as `source: "reviewer"` to slip past `tester_succeeded`
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
                    tracing::warn!(%parse_error, ?role, stdout = %outcome.stdout, "agent produced unparsable or misattributed output");
                    vec![Finding {
                        source: role_to_finding_source(role),
                        severity: warden_core::Severity::Blocking,
                        file: None,
                        description: format!(
                            "{role:?} produced unparsable or misattributed output: {parse_error}"
                        ),
                        action: Some("fix the agent's output format/finding sources".to_string()),
                    }]
                }
            }
        };

        // ADR-0009 (issue #7): capture evidence right after a *successful*
        // tester run, still inside its worktree -- which is about to be
        // removed below, so this must happen before that, not after.
        if role == AgentRole::Tester && tester_succeeded(&findings) {
            // `agent.command` *is* the tester command here: this branch only
            // runs for `AgentRole::Tester`.
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
            tracing::warn!(%error, ?role, "failed to clean up worktree after cycle");
        }

        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::test_support::*;
    use std::process::Command as SyncCommand;
    use tempfile::TempDir;

    /// Acceptance criterion 1 (issue #2), updated for issue #40's
    /// independent `run_review`/`run_test` (the removed `run_review_and_test`
    /// used to exercise this concurrently via `tokio::join!`; reviewer and
    /// tester now run sequentially, see `run_review_and_test_runs_...`
    /// below): reviewer and tester each write to a DIFFERENT file in their
    /// own worktree, then read back the *other* role's target file from
    /// their own worktree. Each gets a fresh worktree checked out from the
    /// same base `commit` (`WorktreeManager::create`, keyed by role), so if
    /// the two ever shared a worktree/directory, the other role's write
    /// would already be visible here instead of the original, untouched
    /// content -- regardless of whether the two run concurrently or in
    /// sequence, this is what distinguishes "isolated worktrees" from
    /// "shared worktree".
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
            coder_agent: definition(AgentCommand::new("sh", ["-c", "true"])),
            reviewer_agent: definition(reviewer_command),
            tester_agent: definition(tester_command),
            evidence_tool: None,
            evidence_store_in_repo: true,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();

        let orchestrator = Orchestrator::new(pool.clone());
        let mut findings = orchestrator
            .run_review(
                &FakeCommandAdapter,
                ReviewInvocation {
                    run_id: "collision-run",
                    cycle_id: "collision-cycle",
                    cycle_number: 1,
                    agent: &agents.reviewer,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        findings.extend(
            orchestrator
                .run_test(
                    &FakeCommandAdapter,
                    TestInvocation {
                        run_id: "collision-run",
                        cycle_id: "collision-cycle",
                        cycle_number: 1,
                        agent: &agents.tester,
                        env_allowlist: agents.env_allowlist,
                        worktree_manager: &worktree_manager,
                        commit: "HEAD",
                        diff: "",
                        prior_findings: &[],
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
            .find(|f| f.source == warden_core::FindingSource::Reviewer)
            .expect("reviewer finding present");
        let tester_finding = findings
            .iter()
            .find(|f| f.source == warden_core::FindingSource::Tester)
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

    /// Issue #40 / decision #37 Q2: a reviewer invoked through `run_review`
    /// with `ReviewScope::Correctif` must receive a payload scoped to the
    /// correctif's own diff plus the findings that prompted it -- captured
    /// directly from what the reviewer agent actually reads off stdin, the
    /// same way `every_role_receives_its_own_definitions_system_prompt_over_stdin`
    /// captures a full-cycle payload.
    #[tokio::test]
    async fn run_review_with_a_correctif_scope_sends_the_reviewer_a_scoped_payload() {
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
            coder_agent: definition(AgentCommand::new("sh", ["-c", "true"])),
            reviewer_agent: definition(capturing_reviewer),
            tester_agent: definition(always_passing_tester()),
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let originating_finding = Finding {
            source: warden_core::FindingSource::Reviewer,
            severity: warden_core::Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "unchecked unwrap".to_string(),
            action: Some("handle the error".to_string()),
        };

        orchestrator
            .run_review(
                &FakeCommandAdapter,
                ReviewInvocation {
                    run_id: "scoped-run",
                    cycle_id: "scoped-cycle",
                    cycle_number: 1,
                    agent: &agents.reviewer,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "diff --git a/x b/x\n+fixed the unwrap\n",
                    prior_findings: std::slice::from_ref(&originating_finding),
                    scope: warden_core::ReviewScope::Correctif,
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

    /// Issue #40: `run_finding_agent` must refuse a `Correctif` scope for
    /// any role but `AgentRole::Reviewer` -- defense in depth against a
    /// future caller that (mis)constructs a `FindingAgentInvocation`
    /// directly instead of going through `run_test` (whose `TestInvocation`
    /// carries no `scope` field at all, so this path can't be reached via
    /// the intended entry points).
    #[tokio::test]
    async fn run_finding_agent_rejects_a_correctif_scope_for_the_tester_role() {
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
            coder_agent: definition(AgentCommand::new("sh", ["-c", "true"])),
            reviewer_agent: definition(always_passing_tester()),
            tester_agent: definition(always_passing_tester()),
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let result = orchestrator
            .run_finding_agent(
                &FakeCommandAdapter,
                FindingAgentInvocation {
                    run_id: "bad-scope-run",
                    cycle_id: "bad-scope-cycle",
                    cycle_number: 1,
                    role: AgentRole::Tester,
                    agent: &agents.tester,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Correctif,
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

    /// Issue #40 (ADR-0003 amendment): reviewer and tester must now run
    /// **sequentially** -- the opposite of what this test asserted before
    /// the removed `run_review_and_test`'s `tokio::join!` path. Regression
    /// coverage for "no one quietly reintroduces `tokio::join!`/`try_join!`
    /// here": `run_review` immediately followed by `run_test`, each backed
    /// by a sleepy agent, must together take at least as long as both
    /// sleeps combined, not just the slower one.
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
            coder_agent: definition(AgentCommand::new("sh", ["-c", "true"])),
            reviewer_agent: definition(sleepy_agent.clone()),
            tester_agent: definition(sleepy_agent),
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
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "timing-cycle", "timing-run", 1)
            .await
            .unwrap();

        let start = std::time::Instant::now();
        orchestrator
            .run_review(
                &FakeCommandAdapter,
                ReviewInvocation {
                    run_id: "timing-run",
                    cycle_id: "timing-cycle",
                    cycle_number: 1,
                    agent: &agents.reviewer,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        orchestrator
            .run_test(
                &FakeCommandAdapter,
                TestInvocation {
                    run_id: "timing-run",
                    cycle_id: "timing-cycle",
                    cycle_number: 1,
                    agent: &agents.tester,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed > SLEEP.mul_f64(1.5),
            "expected run_review then run_test ({elapsed:?}) to together take \
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
    /// `FindingSource::Reviewer` (the role that actually produced this
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
            coder_agent: definition(AgentCommand::new("sh", ["-c", "true"])),
            reviewer_agent: definition(forging_reviewer),
            tester_agent: definition(always_passing_tester()),
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let findings = orchestrator
            .run_finding_agent(
                &FakeCommandAdapter,
                FindingAgentInvocation {
                    run_id: "forge-run",
                    cycle_id: "forge-cycle",
                    cycle_number: 1,
                    role: AgentRole::Reviewer,
                    agent: &agents.reviewer,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].source,
            warden_core::FindingSource::Reviewer,
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
    /// (closing Minor 2, `tester_succeeded` trusting an agent-controlled
    /// `source`): a tester that mislabels its own failure as
    /// `source: "reviewer"` must not have that failure hidden from
    /// `tester_succeeded` -- the gate `run_finding_agent` uses to decide
    /// whether to trigger evidence capture. Before the fix, a forged
    /// `source: "reviewer"` finding from the tester would sail through
    /// `extract_findings` unchanged, and `tester_succeeded` (which only ever
    /// looks for a `FindingSource::Tester` blocking finding) would report
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
            coder_agent: definition(AgentCommand::new("sh", ["-c", "true"])),
            reviewer_agent: definition(always_passing_tester()),
            tester_agent: definition(self_mislabelling_tester),
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let findings = orchestrator
            .run_finding_agent(
                &FakeCommandAdapter,
                FindingAgentInvocation {
                    run_id: "mislabel-run",
                    cycle_id: "mislabel-cycle",
                    cycle_number: 1,
                    role: AgentRole::Tester,
                    agent: &agents.tester,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].source,
            warden_core::FindingSource::Tester,
            "the tester's own mislabelled finding must be re-attributed to Tester, not left as \
                 the forged Reviewer source: {findings:?}"
        );
        assert_eq!(findings[0].severity, warden_core::Severity::Blocking);
        assert!(
            !tester_succeeded(&findings),
            "Minor 2: a tester that mislabels its own failure must still be seen as failed by \
                 tester_succeeded, the gate that decides whether to trigger evidence capture"
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
            coder_agent: definition(AgentCommand::new("sh", ["-c", "true"])),
            reviewer_agent: definition(honest_reviewer),
            tester_agent: definition(always_passing_tester()),
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        };
        let agents = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();
        let orchestrator = Orchestrator::new(pool.clone());

        let findings = orchestrator
            .run_finding_agent(
                &FakeCommandAdapter,
                FindingAgentInvocation {
                    run_id: "legit-run",
                    cycle_id: "legit-cycle",
                    cycle_number: 1,
                    role: AgentRole::Reviewer,
                    agent: &agents.reviewer,
                    env_allowlist: agents.env_allowlist,
                    worktree_manager: &worktree_manager,
                    commit: "HEAD",
                    diff: "",
                    prior_findings: &[],
                    scope: warden_core::ReviewScope::Full,
                    config: &config,
                    cancel: CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            findings,
            vec![Finding {
                source: warden_core::FindingSource::Reviewer,
                severity: warden_core::Severity::Warning,
                file: Some("src/lib.rs".to_string()),
                description: "looks mostly fine".to_string(),
                action: None,
            }]
        );
    }
}
