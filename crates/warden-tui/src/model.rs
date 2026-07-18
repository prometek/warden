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

    /// The run's intent/branch/max_cycles, from its `RunStarted` event --
    /// `None` until that event has been applied (e.g. a late attach whose
    /// history query hasn't returned yet).
    pub fn run_started(&self) -> Option<(&str, &str, u32)> {
        self.events.iter().find_map(|record| match &record.event {
            RunEvent::RunStarted {
                intent,
                branch,
                max_cycles,
            } => Some((intent.as_str(), branch.as_str(), *max_cycles)),
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
                max_cycles: 5,
            },
        ));

        assert_eq!(model.run_id(), Some("run-1"));
        assert_eq!(model.events().len(), 1);
        assert_eq!(model.run_started(), Some(("do the thing", "main", 5)));
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
}
