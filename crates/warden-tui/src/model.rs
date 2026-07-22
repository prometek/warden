//! The pure projection of a run's event stream into whatever [`crate::ui`]
//! renders. No I/O, no terminal, no clock -- [`RunModel::apply`] is a plain
//! synchronous function over already-decoded [`RunEventRecord`]s, fed by
//! [`crate::attach`] (which owns the actual replay/live merge). Kept
//! separate and dependency-free by design (code-standards.md, "TUI
//! (ratatui)": "la couche modèle ... testable sans terminal").

use warden_core::RunEvent;
use warden_core::RunEventRecord;

/// The state of one run, built up by applying its event stream in order.
#[derive(Debug, Clone, Default)]
pub struct RunModel {
    seen_ids: std::collections::HashSet<String>,
    events: Vec<RunEventRecord>,
}

impl RunModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one event to the model. Deduplicates by `id`: a `warden-tui`
    /// that subscribes to the live Event Bus *before* querying `events` for
    /// history (Architecture.md §5.4, to avoid a gap) can see the same
    /// event delivered from both sources, and this must be a no-op the
    /// second time, not a duplicated log line.
    ///
    /// Returns `true` if `record` was newly inserted, `false` if it was
    /// already known (a duplicate). Every caller that turns applied events
    /// into user-visible output -- not just the interactive `app_loop`, but
    /// also the headless NDJSON dump -- must gate on this return value
    /// rather than re-deriving "is this new" some other way, or the two
    /// paths can disagree about what counts as a duplicate.
    pub fn apply(&mut self, record: RunEventRecord) -> bool {
        if !self.seen_ids.insert(record.id.clone()) {
            return false;
        }
        self.events.push(record);
        true
    }

    /// The run this model has observed events for, if any have arrived yet.
    pub fn run_id(&self) -> Option<&str> {
        self.events.first().map(|record| record.run_id.as_str())
    }

    /// Every event applied so far, in the order they were applied -- the
    /// scrollable log view renders this directly.
    pub fn events(&self) -> &[RunEventRecord] {
        &self.events
    }

    /// The most recently started cycle's number, or `0` before any cycle
    /// has started.
    pub fn current_cycle_number(&self) -> u32 {
        self.events
            .iter()
            .rev()
            .find_map(|record| match &record.event {
                RunEvent::CycleStarted { cycle_number } => Some(*cycle_number),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// The run's intent/branch/per-phase budgets (issue #43), from its
    /// `RunStarted` event -- `None` until that event has been applied (e.g. a
    /// late attach whose history query hasn't returned yet).
    pub fn run_started(&self) -> Option<(&str, &str, u32, u32)> {
        self.events.iter().find_map(|record| match &record.event {
            RunEvent::RunStarted {
                intent,
                branch,
                max_review_cycles,
                max_test_cycles,
            } => Some((
                intent.as_str(),
                branch.as_str(),
                *max_review_cycles,
                *max_test_cycles,
            )),
            _ => None,
        })
    }

    /// `true` once a `RunFinished` event has been applied -- the run has
    /// reached a terminal state and nothing further will arrive on the bus.
    pub fn is_finished(&self) -> bool {
        self.final_state().is_some()
    }

    /// The run's terminal [`warden_core::RunState`] (as its stable string
    /// form), from its `RunFinished` event, if the run has finished.
    pub fn final_state(&self) -> Option<&str> {
        self.events
            .iter()
            .rev()
            .find_map(|record| match &record.event {
                RunEvent::RunFinished { final_state } => Some(final_state.as_str()),
                _ => None,
            })
    }

    /// Every `FindingRaised` event applied so far, oldest first.
    pub fn findings(&self) -> impl Iterator<Item = &RunEventRecord> {
        self.events
            .iter()
            .filter(|record| matches!(record.event, RunEvent::FindingRaised { .. }))
    }

    /// The most recently captured evidence, if any -- what `crate::ui`'s
    /// evidence pane shows right now. `None` until an `EvidenceCaptured`
    /// event has actually been applied; nothing in this codebase currently
    /// emits one (Phase 7 / issue #7's Evidence Capture Adapter hasn't
    /// landed), but the rendering path this feeds (`crate::ui`,
    /// `crate::evidence`) is fully wired and exercised the moment one is.
    pub fn latest_evidence(&self) -> Option<&RunEventRecord> {
        self.events
            .iter()
            .rev()
            .find(|record| matches!(record.event, RunEvent::EvidenceCaptured { .. }))
    }

    /// "What is the currently running agent reporting right now" (issue
    /// #33): the `(role, detail)` of the most recent `AgentProgress` event,
    /// but only if no `AgentFinished` has arrived since -- once an agent
    /// finishes, its last progress line is stale and must stop being shown
    /// as current. Scans back from the most recent event and stops at the
    /// first of either variant, so an `AgentFinished` with no intervening
    /// `AgentProgress` correctly reports `None` too (an agent that finished
    /// without ever reporting progress has nothing "current" to show).
    ///
    /// **Live-only** (ADR-0008 amendment, this issue): this can only ever be
    /// non-`None` while attached live -- a late attach's `events` history
    /// replay never contains an `AgentProgress` at all (never persisted),
    /// so this always starts `None` after a pure-history replay and only
    /// becomes populated once a live one actually arrives.
    pub fn current_progress(&self) -> Option<(&str, &str)> {
        for record in self.events.iter().rev() {
            match &record.event {
                RunEvent::AgentProgress { role, detail } => {
                    return Some((role.as_str(), detail.as_str()))
                }
                RunEvent::AgentFinished { .. } => return None,
                _ => continue,
            }
        }
        None
    }

    /// Issue #53: every `(cycle_number, role, usage)` an `AgentFinished`
    /// event has reported non-`None` usage for, oldest first -- the raw
    /// "per agent" facts [`token_usage_by_cycle`](RunModel::token_usage_by_cycle)
    /// and [`total_token_usage`](RunModel::total_token_usage) roll up from.
    /// `cycle_number` is the most recent `CycleStarted` seen before that
    /// `AgentFinished` (`0` before any cycle has started -- not reachable in
    /// a real run, since no agent runs before its own cycle's
    /// `CycleStarted`).
    ///
    /// A tool that reported no usage at all for a given invocation
    /// contributes nothing here (its `AgentFinished { usage: None, .. }` is
    /// simply skipped) rather than a fabricated zero entry -- see
    /// `warden_core::TokenUsage`'s own docs on why "n/a" and "zero" are
    /// different facts.
    pub fn token_usage_entries(&self) -> Vec<(u32, &str, warden_core::TokenUsage)> {
        let mut cycle_number: u32 = 0;
        let mut entries = Vec::new();
        for record in &self.events {
            match &record.event {
                RunEvent::CycleStarted {
                    cycle_number: started,
                } => cycle_number = *started,
                RunEvent::AgentFinished {
                    role,
                    usage: Some(usage),
                    ..
                } => entries.push((cycle_number, role.as_str(), *usage)),
                _ => {}
            }
        }
        entries
    }

    /// [`token_usage_entries`](RunModel::token_usage_entries), rolled up per
    /// cycle -- the "per cycle" half of issue #53's aggregation. Ordered by
    /// first appearance (cycle numbers only ever increase within a run, so
    /// this is already ascending in practice).
    pub fn token_usage_by_cycle(&self) -> Vec<(u32, warden_core::TokenUsage)> {
        let mut by_cycle: Vec<(u32, warden_core::TokenUsage)> = Vec::new();
        for (cycle_number, _role, usage) in self.token_usage_entries() {
            match by_cycle
                .iter_mut()
                .find(|(number, _)| *number == cycle_number)
            {
                Some((_, total)) => *total = total.merge(&usage),
                None => by_cycle.push((cycle_number, usage)),
            }
        }
        by_cycle
    }

    /// The run-wide grand total across every invocation that has reported
    /// usage so far (issue #53) -- `None` until at least one has, never a
    /// fabricated `0` (rendered "n/a" by [`crate::ui`]).
    pub fn total_token_usage(&self) -> Option<warden_core::TokenUsage> {
        let usages: Vec<warden_core::TokenUsage> = self
            .token_usage_entries()
            .into_iter()
            .map(|(_, _, usage)| usage)
            .collect();
        warden_core::TokenUsage::sum(&usages)
    }

    /// Derives the run's workflow tree (issue #54): one branch per cycle,
    /// each carrying its agent-invocation nodes (coder/reviewer/tester, in
    /// the order the orchestrator actually runs them within a cycle --
    /// `warden::orchestrator`'s main loop: coder, then reviewer, then
    /// tester only if the review came back clean) plus, if the cycle
    /// reboucled into another one, *why* (a distinct "return edge" per
    /// issue #54's acceptance criteria).
    ///
    /// Pure projection over already-applied events, recomputed on every
    /// call -- cheap enough at the event volumes a single run produces, and
    /// keeps this in the same "no incremental derived state to keep in
    /// sync" shape as the rest of this module (`token_usage_entries` and
    /// friends).
    pub fn workflow_tree(&self) -> WorkflowTree {
        let mut cycles: Vec<CycleNode> = Vec::new();
        // Issue #37: `RunEvent` carries no explicit phase/cycle field on
        // `AgentStarted`/`AgentFinished` (verified by reading `event.rs` --
        // it doesn't exist), so, exactly like `token_usage_entries` already
        // does, an invocation is attributed to whichever `CycleStarted` most
        // recently preceded it -- the orchestrator always runs a cycle's
        // agents strictly between that cycle's own `CycleStarted` and the
        // next one's, so this is exact, not a heuristic.
        let mut findings_by_cycle: std::collections::HashMap<u32, Vec<(String, String)>> =
            std::collections::HashMap::new();

        for record in &self.events {
            match &record.event {
                RunEvent::CycleStarted { cycle_number } => {
                    cycles.push(CycleNode {
                        cycle_number: *cycle_number,
                        agents: Vec::new(),
                        reloop: None,
                    });
                }
                RunEvent::AgentStarted { role } => {
                    if let Some(cycle) = cycles.last_mut() {
                        cycle.agents.push(AgentNode {
                            role: role.clone(),
                            status: NodeStatus::Running,
                            tokens: None,
                        });
                    }
                }
                RunEvent::AgentFinished {
                    role,
                    exit_code,
                    usage,
                } => {
                    if let Some(cycle) = cycles.last_mut() {
                        let status = if *exit_code == 0 {
                            NodeStatus::Clean
                        } else {
                            NodeStatus::Failed
                        };
                        match cycle
                            .agents
                            .iter_mut()
                            .rev()
                            .find(|node| &node.role == role && node.status == NodeStatus::Running)
                        {
                            Some(node) => {
                                node.status = status;
                                node.tokens = *usage;
                            }
                            // Defensive (code-standards.md: validate at the
                            // boundary, never trust a gap in the stream to
                            // mean "nothing happened"): an `AgentFinished`
                            // with no matching `AgentStarted` in this cycle
                            // -- e.g. a late attach whose history replay
                            // started mid-invocation -- still surfaces as a
                            // node rather than being silently dropped.
                            None => cycle.agents.push(AgentNode {
                                role: role.clone(),
                                status,
                                tokens: *usage,
                            }),
                        }
                    }
                }
                RunEvent::FindingRaised {
                    cycle_number,
                    source,
                    severity,
                    ..
                } => {
                    findings_by_cycle
                        .entry(*cycle_number)
                        .or_default()
                        .push((source.clone(), severity.clone()));
                }
                _ => {}
            }
        }

        // Second pass: elevate a reviewer/tester node from `Clean` to
        // `Findings` where a blocking finding attributed to that role
        // landed in the same cycle -- findings are only known in full once
        // the whole cycle (which may include both the reviewer and the
        // tester) has been walked, so this can't be decided during the
        // first pass above.
        for cycle in &mut cycles {
            if let Some(findings) = findings_by_cycle.get(&cycle.cycle_number) {
                for agent in &mut cycle.agents {
                    if agent.status == NodeStatus::Clean
                        && findings.iter().any(|(source, severity)| {
                            severity == "blocking" && role_owns_finding_source(&agent.role, source)
                        })
                    {
                        agent.status = NodeStatus::Findings;
                    }
                }
            }
        }

        // Third pass: a cycle only actually "returned" if there is a next
        // cycle to return *to* -- a cycle that hit its budget
        // (`MaxReviewCyclesExceeded`/`MaxTestCyclesExceeded`) also raised a
        // blocking finding but the run stopped right there, and drawing a
        // return edge to a cycle that never happened would be fabricating
        // structure the event stream never showed.
        let cycle_count = cycles.len();
        for i in 0..cycle_count {
            if i + 1 >= cycle_count {
                continue;
            }
            let this_cycle_number = cycles[i].cycle_number;
            let next_cycle_number = cycles[i + 1].cycle_number;
            let this_findings = findings_by_cycle.get(&this_cycle_number);
            let review_blocking = this_findings.is_some_and(|findings| {
                findings.iter().any(|(source, severity)| {
                    severity == "blocking" && role_owns_finding_source("reviewer", source)
                })
            });
            let test_blocking = this_findings.is_some_and(|findings| {
                findings
                    .iter()
                    .any(|(source, severity)| severity == "blocking" && source == "tester")
            });
            cycles[i].reloop = if review_blocking {
                Some(ReloopCause::ReviewFinding)
            } else if test_blocking {
                Some(ReloopCause::TestFinding)
            } else {
                // Issue #15/ADR-0011: a `ChecksFailed` CI outcome reboucles
                // to the coder one step later in the pipeline than a
                // reviewer/tester finding does -- its `FindingRaised` (see
                // `warden::orchestrator`, `pending_ci_findings`) is
                // attributed to the *next* cycle it seeds, not to this one,
                // so it's detected by looking there instead.
                let next_cycle_has_ci_finding = findings_by_cycle
                    .get(&next_cycle_number)
                    .is_some_and(|findings| findings.iter().any(|(source, _)| source == "ci"));
                if next_cycle_has_ci_finding {
                    Some(ReloopCause::CiFailure)
                } else {
                    None
                }
            };
        }

        WorkflowTree { cycles }
    }
}

/// `true` if a blocking finding from `source` (a raw `FindingSource::as_str`
/// value, per `warden_core::convergence`) is charged to `role`'s gate --
/// mirrors `warden_core::decide_next_state`'s own imputation rule
/// (`Reviewer`/`Warden` sourced findings gate the reviewer, decision #37
/// Q1's tampering-finding carve-out included; only `Tester`-sourced
/// findings gate the tester). The coder never owns a finding source itself
/// (it doesn't raise findings, only the roles reviewing/testing its work
/// do), so this is always `false` for any other role.
fn role_owns_finding_source(role: &str, source: &str) -> bool {
    match role {
        "reviewer" => source == "reviewer" || source == "warden",
        "tester" => source == "tester",
        _ => false,
    }
}

/// The outcome of one agent invocation node in [`WorkflowTree`] (issue #54).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    /// `AgentStarted` seen, no matching `AgentFinished` yet -- the
    /// invocation is still running.
    Running,
    /// Finished with a zero exit code and, for the reviewer/tester, no
    /// blocking finding attributed to that role in this cycle.
    Clean,
    /// Finished with a zero exit code, but at least one blocking finding
    /// attributed to this role landed in this cycle (reviewer/tester
    /// only -- the coder never carries this status, see
    /// `role_owns_finding_source`).
    Findings,
    /// Finished with a non-zero exit code -- the agent process itself
    /// failed, independent of any finding.
    Failed,
}

/// One agent invocation inside a cycle -- a node in [`WorkflowTree`].
#[derive(Debug, Clone, PartialEq)]
pub struct AgentNode {
    pub role: String,
    pub status: NodeStatus,
    /// Issue #53: `None` both while `status` is [`NodeStatus::Running`]
    /// (no `AgentFinished` yet) and for a tool that reported no usage at
    /// all once finished -- rendered "n/a" either way by [`crate::ui`],
    /// never a fabricated `0`.
    pub tokens: Option<warden_core::TokenUsage>,
}

/// Why a cycle reboucled into the next one -- what makes a "return edge"
/// (issue #54's acceptance criteria: "Reloops (coder<->reviewer, scoped
/// re-review, tester return) are visually distinct") distinguishable from a
/// plain fresh cycle by [`crate::ui`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloopCause {
    /// A reviewer- or tampering-sourced blocking finding (decision #37 Q1)
    /// -- charged to the review budget; the next cycle's reviewer only
    /// re-reviews the coder's latest correctif (`ReviewScope::Correctif`),
    /// never the whole diff again.
    ReviewFinding,
    /// A tester-sourced blocking finding -- charged to the test budget; the
    /// next cycle still runs a scoped re-review before the tester is
    /// allowed to run again (Phase A gate, issue #41), so this reboucle is
    /// coder -> reviewer -> tester, not a direct return to the tester.
    TestFinding,
    /// A `ChecksFailed` CI outcome (issue #15/ADR-0011) reboucling a
    /// post-convergence run back to the coder.
    CiFailure,
}

/// One cycle's worth of agent-invocation nodes, plus (if this cycle
/// reboucled into another one) why.
#[derive(Debug, Clone, PartialEq)]
pub struct CycleNode {
    pub cycle_number: u32,
    pub agents: Vec<AgentNode>,
    pub reloop: Option<ReloopCause>,
}

/// The whole run projected as a tree (issue #54): the run itself is the
/// implicit root: `crate::ui` renders that root from
/// [`RunModel::run_started`] directly, so this only carries the branches
/// (cycles) hanging off it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkflowTree {
    pub cycles: Vec<CycleNode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, event: RunEvent) -> RunEventRecord {
        RunEventRecord {
            id: id.to_string(),
            run_id: "run-1".to_string(),
            event,
            created_at: "2026-07-12T00:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn empty_model_reports_no_run_and_is_not_finished() {
        let model = RunModel::new();
        assert_eq!(model.run_id(), None);
        assert_eq!(model.current_cycle_number(), 0);
        assert!(!model.is_finished());
        assert_eq!(model.final_state(), None);
    }

    #[test]
    fn applying_an_event_exposes_the_run_id_and_event_log() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "do the thing".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 5,
                max_test_cycles: 4,
            },
        ));

        assert_eq!(model.run_id(), Some("run-1"));
        assert_eq!(model.events().len(), 1);
        assert_eq!(model.run_started(), Some(("do the thing", "main", 5, 4)));
    }

    #[test]
    fn applying_the_same_event_id_twice_is_a_no_op() {
        let mut model = RunModel::new();
        let event = record("e1", RunEvent::CycleStarted { cycle_number: 1 });
        model.apply(event.clone());
        model.apply(event);

        assert_eq!(
            model.events().len(),
            1,
            "a duplicate delivery (live + history overlap) must not be logged twice"
        );
    }

    #[test]
    fn current_cycle_number_tracks_the_latest_cycle_started_event() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e2",
            RunEvent::AgentStarted {
                role: "coder".to_string(),
            },
        ));
        model.apply(record("e3", RunEvent::CycleStarted { cycle_number: 2 }));

        assert_eq!(model.current_cycle_number(), 2);
    }

    #[test]
    fn is_finished_and_final_state_reflect_the_run_finished_event() {
        let mut model = RunModel::new();
        assert!(!model.is_finished());

        model.apply(record(
            "e1",
            RunEvent::RunFinished {
                final_state: "converged".to_string(),
            },
        ));

        assert!(model.is_finished());
        assert_eq!(model.final_state(), Some("converged"));
    }

    #[test]
    fn findings_filters_out_every_other_event_kind() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e2",
            RunEvent::FindingRaised {
                cycle_number: 1,
                source: "reviewer".to_string(),
                severity: "blocking".to_string(),
                file: None,
                description: "missing test".to_string(),
                action: None,
            },
        ));
        model.apply(record(
            "e3",
            RunEvent::AgentFinished {
                role: "reviewer".to_string(),
                exit_code: 0,
                usage: None,
            },
        ));

        let findings: Vec<&RunEventRecord> = model.findings().collect();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].id, "e2");
    }

    #[test]
    fn latest_evidence_is_none_until_an_evidence_captured_event_is_applied() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        assert!(model.latest_evidence().is_none());
    }

    #[test]
    fn latest_evidence_tracks_the_most_recently_applied_evidence_captured_event() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::EvidenceCaptured {
                cycle_number: 1,
                evidence_type: "image".to_string(),
                file_path: "first.png".to_string(),
                description: None,
            },
        ));
        model.apply(record("e2", RunEvent::CycleStarted { cycle_number: 2 }));
        model.apply(record(
            "e3",
            RunEvent::EvidenceCaptured {
                cycle_number: 2,
                evidence_type: "image".to_string(),
                file_path: "second.png".to_string(),
                description: None,
            },
        ));

        let latest = model
            .latest_evidence()
            .expect("an evidence event was applied");
        assert_eq!(latest.id, "e3");
        assert!(matches!(
            &latest.event,
            RunEvent::EvidenceCaptured { file_path, .. } if file_path == "second.png"
        ));
    }

    #[test]
    fn current_progress_is_none_before_any_progress_event_has_arrived() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::AgentStarted {
                role: "coder".to_string(),
            },
        ));
        assert_eq!(model.current_progress(), None);
    }

    #[test]
    fn current_progress_tracks_the_most_recently_applied_progress_event() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::AgentProgress {
                role: "coder".to_string(),
                detail: "reading the codebase".to_string(),
            },
        ));
        model.apply(record(
            "e2",
            RunEvent::AgentProgress {
                role: "coder".to_string(),
                detail: "running cargo test".to_string(),
            },
        ));

        assert_eq!(
            model.current_progress(),
            Some(("coder", "running cargo test"))
        );
    }

    /// Issue #33: once an agent has finished, its last progress line is
    /// stale and must stop being shown as "current" -- a header still
    /// reading "running cargo test" after the agent already exited would be
    /// actively misleading, worse than showing nothing.
    #[test]
    fn current_progress_is_cleared_once_the_agent_finishes() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::AgentProgress {
                role: "coder".to_string(),
                detail: "running cargo test".to_string(),
            },
        ));
        model.apply(record(
            "e2",
            RunEvent::AgentFinished {
                role: "coder".to_string(),
                exit_code: 0,
                usage: None,
            },
        ));

        assert_eq!(model.current_progress(), None);
    }

    /// A fresh agent's own progress, arriving after a previous agent's
    /// `AgentFinished`, must be shown again -- `current_progress` isn't
    /// permanently latched off by the first finish it ever sees.
    #[test]
    fn current_progress_resumes_once_a_new_agent_reports_progress_after_a_prior_one_finished() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::AgentProgress {
                role: "coder".to_string(),
                detail: "coder work".to_string(),
            },
        ));
        model.apply(record(
            "e2",
            RunEvent::AgentFinished {
                role: "coder".to_string(),
                exit_code: 0,
                usage: None,
            },
        ));
        model.apply(record(
            "e3",
            RunEvent::AgentProgress {
                role: "reviewer".to_string(),
                detail: "reviewing the diff".to_string(),
            },
        ));

        assert_eq!(
            model.current_progress(),
            Some(("reviewer", "reviewing the diff"))
        );
    }

    // -----------------------------------------------------------------
    // Token usage aggregation (issue #53)
    // -----------------------------------------------------------------

    fn agent_finished(role: &str, usage: Option<warden_core::TokenUsage>) -> RunEvent {
        RunEvent::AgentFinished {
            role: role.to_string(),
            exit_code: 0,
            usage,
        }
    }

    #[test]
    fn token_usage_entries_is_empty_when_nothing_has_reported_usage() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_finished("coder", None)));

        assert!(model.token_usage_entries().is_empty());
        assert_eq!(model.total_token_usage(), None);
    }

    #[test]
    fn token_usage_entries_attributes_each_reported_usage_to_the_role_and_current_cycle() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e2",
            agent_finished(
                "coder",
                Some(warden_core::TokenUsage::new(100, 50, None, None)),
            ),
        ));
        model.apply(record("e3", RunEvent::CycleStarted { cycle_number: 2 }));
        model.apply(record(
            "e4",
            agent_finished(
                "reviewer",
                Some(warden_core::TokenUsage::new(30, 10, Some(5), None)),
            ),
        ));

        let entries = model.token_usage_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, 1);
        assert_eq!(entries[0].1, "coder");
        assert_eq!(
            entries[0].2,
            warden_core::TokenUsage::new(100, 50, None, None)
        );
        assert_eq!(entries[1].0, 2);
        assert_eq!(entries[1].1, "reviewer");
    }

    #[test]
    fn token_usage_by_cycle_rolls_up_every_role_reporting_usage_within_the_same_cycle() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e2",
            agent_finished(
                "coder",
                Some(warden_core::TokenUsage::new(100, 50, None, None)),
            ),
        ));
        model.apply(record(
            "e3",
            agent_finished(
                "reviewer",
                Some(warden_core::TokenUsage::new(30, 10, None, None)),
            ),
        ));

        let by_cycle = model.token_usage_by_cycle();
        assert_eq!(by_cycle.len(), 1);
        assert_eq!(by_cycle[0].0, 1);
        assert_eq!(by_cycle[0].1.input_tokens, 130);
        assert_eq!(by_cycle[0].1.output_tokens, 60);
    }

    #[test]
    fn total_token_usage_sums_across_every_cycle_and_role() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e2",
            agent_finished(
                "coder",
                Some(warden_core::TokenUsage::new(100, 50, None, None)),
            ),
        ));
        model.apply(record("e3", RunEvent::CycleStarted { cycle_number: 2 }));
        model.apply(record(
            "e4",
            agent_finished(
                "reviewer",
                Some(warden_core::TokenUsage::new(30, 10, Some(5), None)),
            ),
        ));

        let total = model.total_token_usage().unwrap();
        assert_eq!(total.input_tokens, 130);
        assert_eq!(total.output_tokens, 60);
        assert_eq!(total.cache_read_tokens, Some(5));
    }

    // -----------------------------------------------------------------
    // Workflow tree derivation (issue #54)
    // -----------------------------------------------------------------

    fn agent_started(role: &str) -> RunEvent {
        RunEvent::AgentStarted {
            role: role.to_string(),
        }
    }

    fn agent_finished_with_exit(
        role: &str,
        exit_code: i32,
        usage: Option<warden_core::TokenUsage>,
    ) -> RunEvent {
        RunEvent::AgentFinished {
            role: role.to_string(),
            exit_code,
            usage,
        }
    }

    fn finding(cycle_number: u32, source: &str, severity: &str) -> RunEvent {
        RunEvent::FindingRaised {
            cycle_number,
            source: source.to_string(),
            severity: severity.to_string(),
            file: None,
            description: "some finding".to_string(),
            action: None,
        }
    }

    #[test]
    fn workflow_tree_is_empty_before_any_cycle_has_started() {
        let model = RunModel::new();
        assert!(model.workflow_tree().cycles.is_empty());
    }

    #[test]
    fn workflow_tree_builds_one_branch_per_cycle_with_its_agent_nodes_in_order() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_started("coder")));
        model.apply(record(
            "e3",
            agent_finished_with_exit(
                "coder",
                0,
                Some(warden_core::TokenUsage::new(100, 50, None, None)),
            ),
        ));
        model.apply(record("e4", agent_started("reviewer")));
        model.apply(record("e5", agent_finished_with_exit("reviewer", 0, None)));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles.len(), 1);
        let cycle = &tree.cycles[0];
        assert_eq!(cycle.cycle_number, 1);
        assert_eq!(cycle.agents.len(), 2);
        assert_eq!(cycle.agents[0].role, "coder");
        assert_eq!(cycle.agents[0].status, NodeStatus::Clean);
        assert_eq!(
            cycle.agents[0].tokens,
            Some(warden_core::TokenUsage::new(100, 50, None, None))
        );
        assert_eq!(cycle.agents[1].role, "reviewer");
        assert_eq!(cycle.agents[1].status, NodeStatus::Clean);
        assert_eq!(
            cycle.agents[1].tokens, None,
            "degrades to n/a, not a fabricated 0"
        );
        assert_eq!(
            cycle.reloop, None,
            "no next cycle, no findings -- nothing reboucled"
        );
    }

    #[test]
    fn workflow_tree_shows_an_invocation_still_running_as_such() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_started("coder")));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles[0].agents[0].status, NodeStatus::Running);
        assert_eq!(tree.cycles[0].agents[0].tokens, None);
    }

    #[test]
    fn workflow_tree_marks_a_nonzero_exit_as_failed_regardless_of_findings() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_started("coder")));
        model.apply(record("e3", agent_finished_with_exit("coder", 1, None)));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles[0].agents[0].status, NodeStatus::Failed);
    }

    /// A blocking reviewer finding elevates the reviewer node to `Findings`
    /// and reboucles review-driven (decision #37 Q1) -- the coder never
    /// carries a `Findings` status, only a role that actually raises
    /// findings does.
    #[test]
    fn workflow_tree_attributes_a_blocking_reviewer_finding_to_the_reviewer_node_and_reloops() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_started("coder")));
        model.apply(record("e3", agent_finished_with_exit("coder", 0, None)));
        model.apply(record("e4", agent_started("reviewer")));
        model.apply(record("e5", agent_finished_with_exit("reviewer", 0, None)));
        model.apply(record("e6", finding(1, "reviewer", "blocking")));
        model.apply(record("e7", RunEvent::CycleStarted { cycle_number: 2 }));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles.len(), 2);
        assert_eq!(
            tree.cycles[0].agents[0].status,
            NodeStatus::Clean,
            "coder is never a finding source"
        );
        assert_eq!(tree.cycles[0].agents[1].status, NodeStatus::Findings);
        assert_eq!(tree.cycles[0].reloop, Some(ReloopCause::ReviewFinding));
    }

    /// A tampering finding (`FindingSource::Warden`) is charged to the
    /// review gate exactly like a real reviewer finding (decision #37 Q1's
    /// carve-out, code review issue #24 M4).
    #[test]
    fn workflow_tree_attributes_a_warden_sourced_tampering_finding_to_the_review_reloop() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_started("reviewer")));
        model.apply(record("e3", agent_finished_with_exit("reviewer", 0, None)));
        model.apply(record("e4", finding(1, "warden", "blocking")));
        model.apply(record("e5", RunEvent::CycleStarted { cycle_number: 2 }));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles[0].agents[0].status, NodeStatus::Findings);
        assert_eq!(tree.cycles[0].reloop, Some(ReloopCause::ReviewFinding));
    }

    /// Phase B (issue #42): a review-clean cycle whose tester raises a
    /// blocking finding reboucles test-driven, distinct from a review
    /// reloop -- the tester node (not the reviewer) is the one elevated.
    #[test]
    fn workflow_tree_attributes_a_blocking_tester_finding_to_the_tester_node_and_reloops() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_started("coder")));
        model.apply(record("e3", agent_finished_with_exit("coder", 0, None)));
        model.apply(record("e4", agent_started("reviewer")));
        model.apply(record("e5", agent_finished_with_exit("reviewer", 0, None)));
        model.apply(record("e6", agent_started("tester")));
        model.apply(record("e7", agent_finished_with_exit("tester", 0, None)));
        model.apply(record("e8", finding(1, "tester", "blocking")));
        model.apply(record("e9", RunEvent::CycleStarted { cycle_number: 2 }));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles[0].agents[1].role, "reviewer");
        assert_eq!(tree.cycles[0].agents[1].status, NodeStatus::Clean);
        assert_eq!(tree.cycles[0].agents[2].role, "tester");
        assert_eq!(tree.cycles[0].agents[2].status, NodeStatus::Findings);
        assert_eq!(tree.cycles[0].reloop, Some(ReloopCause::TestFinding));
    }

    /// A non-blocking (warning) finding must never elevate a node's status
    /// or trigger a reloop -- only a `Severity::Blocking` finding gates
    /// anything (`warden_core::decide_next_state`'s own rule).
    #[test]
    fn workflow_tree_ignores_a_non_blocking_finding() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_started("reviewer")));
        model.apply(record("e3", agent_finished_with_exit("reviewer", 0, None)));
        model.apply(record("e4", finding(1, "reviewer", "warning")));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles[0].agents[0].status, NodeStatus::Clean);
        assert_eq!(tree.cycles[0].reloop, None);
    }

    /// A cycle that hit its budget still raised a blocking finding, but the
    /// run never actually reboucled (no next `CycleStarted` follows) -- a
    /// return edge to a cycle that never happened would be fabricated
    /// structure, not something the event stream actually showed.
    #[test]
    fn workflow_tree_shows_no_reloop_when_a_blocking_finding_is_this_run_s_last_cycle() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_started("reviewer")));
        model.apply(record("e3", agent_finished_with_exit("reviewer", 0, None)));
        model.apply(record("e4", finding(1, "reviewer", "blocking")));
        model.apply(record(
            "e5",
            RunEvent::RunFinished {
                final_state: "max_review_cycles_exceeded".to_string(),
            },
        ));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles.len(), 1);
        assert_eq!(tree.cycles[0].agents[0].status, NodeStatus::Findings);
        assert_eq!(tree.cycles[0].reloop, None);
    }

    /// Issue #15/ADR-0011: a `ChecksFailed` CI reboucle's `FindingRaised` is
    /// attributed to the cycle it seeds (the next one), not the converged
    /// cycle it reboucles from -- so the return edge on the *prior* cycle
    /// must still be detected from there.
    #[test]
    fn workflow_tree_detects_a_ci_driven_reloop_seeded_into_the_next_cycle() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_started("reviewer")));
        model.apply(record("e3", agent_finished_with_exit("reviewer", 0, None)));
        model.apply(record("e4", agent_started("tester")));
        model.apply(record("e5", agent_finished_with_exit("tester", 0, None)));
        // Cycle 1 itself is entirely clean -- it only reboucles because of
        // what CI reported after convergence.
        model.apply(record("e6", RunEvent::CycleStarted { cycle_number: 2 }));
        model.apply(record("e7", finding(2, "ci", "blocking")));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles[0].reloop, Some(ReloopCause::CiFailure));
    }

    /// An `AgentFinished` with no preceding `AgentStarted` in the same
    /// cycle (a late attach whose history replay started mid-invocation)
    /// still surfaces as a node instead of being silently dropped.
    #[test]
    fn workflow_tree_surfaces_an_agent_finished_with_no_matching_started_event() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_finished_with_exit("coder", 0, None)));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles[0].agents.len(), 1);
        assert_eq!(tree.cycles[0].agents[0].role, "coder");
        assert_eq!(tree.cycles[0].agents[0].status, NodeStatus::Clean);
    }

    /// A converged run's final cycle (review clean, test clean, no
    /// following cycle) never reboucles.
    #[test]
    fn workflow_tree_shows_no_reloop_for_a_fully_clean_final_cycle() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_started("coder")));
        model.apply(record("e3", agent_finished_with_exit("coder", 0, None)));
        model.apply(record("e4", agent_started("reviewer")));
        model.apply(record("e5", agent_finished_with_exit("reviewer", 0, None)));
        model.apply(record("e6", agent_started("tester")));
        model.apply(record("e7", agent_finished_with_exit("tester", 0, None)));
        model.apply(record(
            "e8",
            RunEvent::RunFinished {
                final_state: "converged".to_string(),
            },
        ));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles.len(), 1);
        assert_eq!(tree.cycles[0].reloop, None);
    }
}
