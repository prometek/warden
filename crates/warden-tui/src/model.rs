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
    pub fn apply(&mut self, record: RunEventRecord) {
        if !self.seen_ids.insert(record.id.clone()) {
            return;
        }
        self.events.push(record);
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
}
