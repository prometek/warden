//! The pure projection of a run's event stream into whatever [`crate::ui`] renders.

use warden_core::RunEvent;
use warden_core::RunEventHistoryEntry;
use warden_core::RunEventRecord;
use warden_core::UndecodableEvent;

/// The state of one run, built up by applying its event stream in order.
#[derive(Debug, Clone, Default)]
pub struct RunModel {
    seen_ids: std::collections::HashSet<String>,
    events: Vec<RunEventRecord>,
    undecodable: Vec<UndecodableEvent>,
}

impl RunModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, record: RunEventRecord) -> bool {
        if !self.seen_ids.insert(record.id.clone()) {
            return false;
        }
        self.events.push(record);
        true
    }

    /// Records one `events` history row that could not be decoded -- never silently dropped, so
    /// [`Self::history`] can still surface it to a renderer.
    pub fn apply_undecodable(&mut self, event: UndecodableEvent) -> bool {
        if !self.seen_ids.insert(event.id.clone()) {
            return false;
        }
        self.undecodable.push(event);
        true
    }

    pub fn apply_history_entry(&mut self, entry: RunEventHistoryEntry) -> bool {
        match entry {
            RunEventHistoryEntry::Decoded(record) => self.apply(record),
            RunEventHistoryEntry::Undecodable(event) => self.apply_undecodable(event),
        }
    }

    /// The run this model has observed events for, if any have arrived yet.
    pub fn run_id(&self) -> Option<&str> {
        self.events
            .first()
            .map(|record| record.run_id.as_str())
            .or_else(|| self.undecodable.first().map(|event| event.run_id.as_str()))
    }

    /// Every event applied so far, in the order they were applied -- the scrollable log view
    /// renders this directly.
    pub fn events(&self) -> &[RunEventRecord] {
        &self.events
    }

    /// Every `events` history row that failed to decode/validate, in the order they were applied --
    /// see [`Self::apply_undecodable`].
    pub fn undecodable_events(&self) -> &[UndecodableEvent] {
        &self.undecodable
    }

    pub fn history(&self) -> Vec<HistoryItem<'_>> {
        let mut items: Vec<HistoryItem> = self
            .events
            .iter()
            .map(HistoryItem::Event)
            .chain(self.undecodable.iter().map(HistoryItem::Undecodable))
            .collect();
        items.sort_by_key(|item| (item.created_at(), item.id()));
        items
    }

    /// The most recently started cycle's number, or `0` before any cycle has started.
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

    /// `true` once the latest event is a terminal `RunFinished` event.
    pub fn is_finished(&self) -> bool {
        self.final_state().is_some_and(|state| {
            !matches!(
                warden_core::RunState::parse(state),
                Ok(warden_core::RunState::AwaitingQuotaReset { .. })
            )
        })
    }

    /// The run's current final-state record, when `RunFinished` is its latest event.
    pub fn final_state(&self) -> Option<&str> {
        self.events.last().and_then(|record| match &record.event {
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

    /// The most recently captured evidence, if any -- what `crate::ui`'s evidence pane shows right
    /// now.
    pub fn latest_evidence(&self) -> Option<&RunEventRecord> {
        self.events
            .iter()
            .rev()
            .find(|record| matches!(record.event, RunEvent::EvidenceCaptured { .. }))
    }

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

    /// The run-wide grand total across every invocation that has reported usage so far -- `None`
    /// until at least one has, never a fabricated `0` (rendered "n/a" by [`crate::ui`]).
    pub fn total_token_usage(&self) -> Option<warden_core::TokenUsage> {
        let usages: Vec<warden_core::TokenUsage> = self
            .token_usage_entries()
            .into_iter()
            .map(|(_, _, usage)| usage)
            .collect();
        warden_core::TokenUsage::sum(&usages)
    }

    /// Last quota report delivered by the tool CLI, if that CLI exposes one.
    pub fn latest_rate_limit_status(&self) -> Option<&warden_core::RateLimitStatus> {
        self.events
            .iter()
            .rev()
            .find_map(|record| match &record.event {
                RunEvent::RateLimitStatusUpdated { status, .. } => Some(status),
                _ => None,
            })
    }

    /// Reset instant carried by the final `AwaitingQuotaReset` state, if the run is suspended.
    pub fn quota_suspension_resets_at(&self) -> Option<i64> {
        self.final_state()
            .and_then(|state| warden_core::RunState::parse(state).ok())
            .and_then(|state| match state {
                warden_core::RunState::AwaitingQuotaReset { resets_at } => Some(resets_at),
                _ => None,
            })
    }

    /// Derives the run's workflow tree: one branch per cycle, each carrying its agent-invocation
    /// nodes plus, if the cycle reboucled into another one, *why*.
    pub fn workflow_tree(&self) -> WorkflowTree {
        let mut cycles: Vec<CycleNode> = Vec::new();
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

#[derive(Debug, Clone, Copy)]
pub enum HistoryItem<'a> {
    Event(&'a RunEventRecord),
    Undecodable(&'a UndecodableEvent),
}

impl<'a> HistoryItem<'a> {
    fn created_at(self) -> &'a str {
        match self {
            HistoryItem::Event(record) => &record.created_at,
            HistoryItem::Undecodable(event) => &event.created_at,
        }
    }

    fn id(self) -> &'a str {
        match self {
            HistoryItem::Event(record) => &record.id,
            HistoryItem::Undecodable(event) => &event.id,
        }
    }
}

/// `true` if a blocking finding from `source` (a raw `FindingSource::as_str` value, per
/// `warden_core::convergence`) is charged to `role`'s gate.
fn role_owns_finding_source(role: &str, source: &str) -> bool {
    match role {
        "reviewer" => source == "reviewer" || source == "warden",
        "tester" => source == "tester",
        _ => false,
    }
}

/// The outcome of one agent invocation node in [`WorkflowTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    /// `AgentStarted` seen, no matching `AgentFinished` yet -- the invocation is still running.
    Running,
    Clean,
    /// Finished with a zero exit code, but at least one blocking finding attributed to this role
    /// landed in this cycle.
    Findings,
    /// Finished with a non-zero exit code -- the agent process itself failed, independent of any
    /// finding.
    Failed,
}

/// One agent invocation inside a cycle -- a node in [`WorkflowTree`].
#[derive(Debug, Clone, PartialEq)]
pub struct AgentNode {
    pub role: String,
    pub status: NodeStatus,
    /// `None` both while `status` is [`NodeStatus::Running`] (no `AgentFinished` yet) and for a
    /// tool that reported no usage at all once finished.
    pub tokens: Option<warden_core::TokenUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloopCause {
    ReviewFinding,
    TestFinding,
    /// A `ChecksFailed` CI outcome reboucling a post-convergence run back to the coder.
    CiFailure,
}

/// One cycle's worth of agent-invocation nodes, plus (if this cycle reboucled into another one)
/// why.
#[derive(Debug, Clone, PartialEq)]
pub struct CycleNode {
    pub cycle_number: u32,
    pub agents: Vec<AgentNode>,
    pub reloop: Option<ReloopCause>,
}

/// The whole run projected as a tree: the run itself is the implicit root.
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
    fn quota_snapshot_and_suspension_are_derived_from_live_events() {
        let mut model = RunModel::new();
        let status = warden_core::RateLimitStatus::new(
            warden_core::RateLimitState::AllowedWarning,
            warden_core::RateLimitWindow::SevenDay,
            0.93,
            false,
            0.75,
            1_785_686_400,
        );
        model.apply(record(
            "e1",
            RunEvent::RateLimitStatusUpdated {
                role: "coder".to_string(),
                status: status.clone(),
            },
        ));
        assert_eq!(model.latest_rate_limit_status(), Some(&status));
        assert_eq!(model.quota_suspension_resets_at(), None);

        model.apply(record(
            "e2",
            RunEvent::RunFinished {
                final_state: warden_core::RunState::AwaitingQuotaReset {
                    resets_at: status.resets_at,
                }
                .as_str(),
            },
        ));
        assert_eq!(model.quota_suspension_resets_at(), Some(status.resets_at));
        assert!(!model.is_finished(), "quota suspension is resumable");

        model.apply(record("e3", RunEvent::CycleStarted { cycle_number: 2 }));
        assert_eq!(
            model.final_state(),
            None,
            "resumed events supersede suspension"
        );
        assert_eq!(model.quota_suspension_resets_at(), None);
    }

    fn undecodable(id: &str, created_at: &str) -> UndecodableEvent {
        UndecodableEvent {
            id: id.to_string(),
            run_id: "run-1".to_string(),
            event_type: "run_finished".to_string(),
            reason: warden_core::UndecodableReason::KindMismatch {
                payload_kind: "cycle_started".to_string(),
            },
            created_at: created_at.to_string(),
        }
    }

    #[test]
    fn apply_undecodable_surfaces_the_row_without_touching_the_decoded_event_log() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply_undecodable(undecodable("e2", "2026-07-12T00:00:01+00:00"));

        assert_eq!(
            model.events().len(),
            1,
            "undecodable rows never join events()"
        );
        assert_eq!(model.undecodable_events().len(), 1);
        assert_eq!(model.undecodable_events()[0].id, "e2");
    }

    #[test]
    fn apply_undecodable_deduplicates_by_id() {
        let mut model = RunModel::new();
        let event = undecodable("e1", "2026-07-12T00:00:00+00:00");
        model.apply_undecodable(event.clone());
        model.apply_undecodable(event);

        assert_eq!(model.undecodable_events().len(), 1);
    }

    #[test]
    fn apply_history_entry_dispatches_decoded_and_undecodable_rows_correctly() {
        let mut model = RunModel::new();
        model.apply_history_entry(RunEventHistoryEntry::Decoded(record(
            "e1",
            RunEvent::CycleStarted { cycle_number: 1 },
        )));
        model.apply_history_entry(RunEventHistoryEntry::Undecodable(undecodable(
            "e2",
            "2026-07-12T00:00:01+00:00",
        )));
        model.apply_history_entry(RunEventHistoryEntry::Decoded(record(
            "e3",
            RunEvent::RunFinished {
                final_state: "converged".to_string(),
            },
        )));

        assert_eq!(
            model.events().len(),
            2,
            "the two decoded rows must both apply"
        );
        assert_eq!(model.undecodable_events().len(), 1);
        assert!(model.is_finished(), "RunFinished must still be reachable");
    }

    #[test]
    fn history_merges_events_and_undecodable_rows_by_created_at() {
        let mut model = RunModel::new();
        model.apply(RunEventRecord {
            id: "e1".to_string(),
            run_id: "run-1".to_string(),
            event: RunEvent::CycleStarted { cycle_number: 1 },
            created_at: "2026-07-12T00:00:00+00:00".to_string(),
        });
        model.apply_undecodable(undecodable("e2", "2026-07-12T00:00:01+00:00"));
        model.apply(RunEventRecord {
            id: "e3".to_string(),
            run_id: "run-1".to_string(),
            event: RunEvent::CycleStarted { cycle_number: 2 },
            created_at: "2026-07-12T00:00:02+00:00".to_string(),
        });

        let ids: Vec<&str> = model.history().iter().map(|item| item.id()).collect();
        assert_eq!(ids, vec!["e1", "e2", "e3"]);
    }

    #[test]
    fn run_id_falls_back_to_an_undecodable_row_when_no_event_decoded_at_all() {
        let mut model = RunModel::new();
        model.apply_undecodable(undecodable("e1", "2026-07-12T00:00:00+00:00"));

        assert_eq!(model.run_id(), Some("run-1"));
    }

    #[test]
    fn a_run_whose_entire_history_is_undecodable_still_reports_a_coherent_empty_view() {
        let mut model = RunModel::new();
        model.apply_undecodable(undecodable("e1", "2026-07-12T00:00:00+00:00"));
        model.apply_undecodable(undecodable("e2", "2026-07-12T00:00:01+00:00"));
        model.apply_undecodable(undecodable("e3", "2026-07-12T00:00:02+00:00"));

        assert_eq!(model.run_id(), Some("run-1"));
        assert!(model.events().is_empty(), "no row ever decoded");
        assert_eq!(model.undecodable_events().len(), 3);
        assert!(!model.is_finished(), "no RunFinished was ever decoded");
        assert_eq!(model.final_state(), None);
        assert!(model.workflow_tree().cycles.is_empty());
        let ids: Vec<&str> = model.history().iter().map(|item| item.id()).collect();
        assert_eq!(ids, vec!["e1", "e2", "e3"]);
    }

    #[test]
    fn history_keeps_a_leading_and_trailing_undecodable_row_in_their_chronological_place() {
        let mut model = RunModel::new();
        model.apply_undecodable(undecodable("e1-first", "2026-07-12T00:00:00+00:00"));
        model.apply(record(
            "e2",
            RunEvent::RunStarted {
                intent: "do the thing".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 5,
                max_test_cycles: 5,
            },
        ));
        model.apply(record(
            "e3",
            RunEvent::RunFinished {
                final_state: "converged".to_string(),
            },
        ));
        model.apply_undecodable(undecodable("e4-last", "2026-07-12T00:00:03+00:00"));

        let ids: Vec<&str> = model.history().iter().map(|item| item.id()).collect();
        assert_eq!(ids, vec!["e1-first", "e2", "e3", "e4-last"]);
        assert!(
            matches!(model.history()[0], HistoryItem::Undecodable(_)),
            "the run's first row is undecodable"
        );
        assert!(
            matches!(model.history()[3], HistoryItem::Undecodable(_)),
            "the run's last row is undecodable"
        );
        assert!(model.is_finished());
        assert_eq!(model.final_state(), Some("converged"));
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

    #[test]
    fn workflow_tree_detects_a_ci_driven_reloop_seeded_into_the_next_cycle() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record("e2", agent_started("reviewer")));
        model.apply(record("e3", agent_finished_with_exit("reviewer", 0, None)));
        model.apply(record("e4", agent_started("tester")));
        model.apply(record("e5", agent_finished_with_exit("tester", 0, None)));
        model.apply(record("e6", RunEvent::CycleStarted { cycle_number: 2 }));
        model.apply(record("e7", finding(2, "ci", "blocking")));

        let tree = model.workflow_tree();
        assert_eq!(tree.cycles[0].reloop, Some(ReloopCause::CiFailure));
    }

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
