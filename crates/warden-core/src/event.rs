//! Run events published on the Event Bus and persisted to `EVENTS` for replay.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

/// Discriminant for a [`RunEvent`], stored separately as `EVENTS.event_type` so it can be
/// filtered/indexed in SQL without deserializing every `payload_json` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    RunStarted,
    /// The workflow's resolved graph (issue #107), published exactly once per run, right after
    /// `RunStarted` and before the first step transition -- so a late attach still learns the full
    /// graph, including steps never reached.
    WorkflowResolved,
    CycleStarted,
    AgentStarted,
    /// A declarative progress signal, translated by the run's `warden::tool_adapter::ToolAdapter`
    /// from one line of an agent's streamed output. Persisted like every other event since issue
    /// #108 (it was live-only before), but *excluded from replay by default* -- see
    /// [`ProgressReplay`] for why, and `warden::progress_writer` for how it reaches the table
    /// without ever blocking the agent. Persisted or not, it stays declarative: what the agent
    /// *reports* doing, never probative evidence (ADR-0009).
    AgentProgress,
    AgentFinished,
    FindingRaised,
    /// Modeled now for forward compatibility with even though nothing in this codebase can produce
    /// one yet.
    EvidenceCaptured,
    UntrustedAgentDefinitionUsed,
    RateLimitStatusUpdated,
    RunFinished,
    /// A lifecycle hook (issue #106) emitted findings at a point with no workflow step to route
    /// them through (`OnRunStart`/`BeforeStep`/`OnConverged`/`BeforePush`) -- recorded so the
    /// variant is never silently dropped, but distinct from [`EventKind::FindingRaised`], which is
    /// always cycle-scoped and can drive a reboucle.
    HookFindingEmitted,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::RunStarted => "run_started",
            EventKind::WorkflowResolved => "workflow_resolved",
            EventKind::CycleStarted => "cycle_started",
            EventKind::AgentStarted => "agent_started",
            EventKind::AgentProgress => "agent_progress",
            EventKind::AgentFinished => "agent_finished",
            EventKind::FindingRaised => "finding_raised",
            EventKind::EvidenceCaptured => "evidence_captured",
            EventKind::UntrustedAgentDefinitionUsed => "untrusted_agent_definition_used",
            EventKind::RateLimitStatusUpdated => "rate_limit_status_updated",
            EventKind::RunFinished => "run_finished",
            EventKind::HookFindingEmitted => "hook_finding_emitted",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "run_started" => Ok(EventKind::RunStarted),
            "workflow_resolved" => Ok(EventKind::WorkflowResolved),
            "cycle_started" => Ok(EventKind::CycleStarted),
            "agent_started" => Ok(EventKind::AgentStarted),
            "agent_progress" => Ok(EventKind::AgentProgress),
            "agent_finished" => Ok(EventKind::AgentFinished),
            "finding_raised" => Ok(EventKind::FindingRaised),
            "evidence_captured" => Ok(EventKind::EvidenceCaptured),
            "untrusted_agent_definition_used" => Ok(EventKind::UntrustedAgentDefinitionUsed),
            "rate_limit_status_updated" => Ok(EventKind::RateLimitStatusUpdated),
            "run_finished" => Ok(EventKind::RunFinished),
            "hook_finding_emitted" => Ok(EventKind::HookFindingEmitted),
            other => Err(CoreError::UnknownEventKind(other.to_string())),
        }
    }
}

/// Whether a history replay of the `events` table returns [`EventKind::AgentProgress`] rows
/// (issue #108).
///
/// Progress outnumbers every other event kind by an order of magnitude -- one event per assistant
/// turn, `tool_use` blocks included -- and no replay reader paginates: `warden-tui` holds the whole
/// history in memory. Excluding it by default keeps an attach as cheap as it was before progress
/// was persisted at all; a reader that actually wants the elapsed progress of a run says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProgressReplay {
    /// Default: `agent_progress` rows are filtered out in SQL.
    #[default]
    Excluded,
    /// Opt-in: `agent_progress` rows are returned inline, in publication order, exactly as a live
    /// subscriber saw them (up to the per-invocation persistence cap).
    Included,
}

impl ProgressReplay {
    pub fn includes_progress(self) -> bool {
        matches!(self, ProgressReplay::Included)
    }
}

/// One declared step of a [`RunEvent::WorkflowResolved`] graph. Deliberately a flat, string-keyed
/// shape independent of `warden_core::workflow` -- see [`RunEvent::WorkflowResolved`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStepWire {
    /// This step's position in `Workflow::steps`, matched against `RunState::RunningStep`.
    pub index: u32,
    /// The step's own id -- `Role::as_str`, and the `role` field of `AgentStarted`/`AgentFinished`
    /// for this step.
    pub id: String,
    /// `"agent"` or `"command"` (`StepKind::as_str`).
    pub kind: String,
    /// Transition target on a clean outcome: another step's `id`, or `"converged"` / `"failed"`.
    pub on_clean: String,
    /// Transition target on a blocking-finding outcome: another step's `id`, or `"converged"` /
    /// `"failed"`.
    pub on_blocking: String,
    /// Transition target on an error outcome: another step's `id`, or `"converged"` / `"failed"`.
    pub on_error: String,
    /// This step's own cycle budget, if narrower than the run-wide `max_cycles`.
    pub max_cycles: Option<u32>,
    pub captures_evidence: bool,
}

/// One structured run transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEvent {
    RunStarted {
        intent: String,
        branch: String,
        #[serde(alias = "max_review_cycles")]
        max_cycles: u32,
    },
    /// See [`EventKind::WorkflowResolved`]. Carries the workflow's own resolved graph -- data only,
    /// no I/O and no dependency on `warden_core::workflow` types themselves, so a non-warden reader
    /// of the `events` table (`jq`, an external script) can still parse it as plain JSON. A
    /// `warden-tui` built before issue #107 does *not* benefit from this: it fails
    /// `EventKind::parse("workflow_resolved")` and tags the line `undecodable` -- a clean
    /// degradation (exit 0), not a successful parse.
    WorkflowResolved {
        name: String,
        entry: u32,
        steps: Vec<WorkflowStepWire>,
    },
    CycleStarted {
        cycle_number: u32,
    },
    AgentStarted {
        role: String,
    },
    AgentProgress {
        role: String,
        detail: String,
    },
    AgentFinished {
        role: String,
        exit_code: i32,
        #[serde(default)]
        usage: Option<crate::TokenUsage>,
    },
    FindingRaised {
        cycle_number: u32,
        source: String,
        severity: String,
        file: Option<String>,
        description: String,
        action: Option<String>,
    },
    /// See [`EventKind::EvidenceCaptured`]: modeled, not yet produced by anything in this codebase.
    EvidenceCaptured {
        cycle_number: u32,
        evidence_type: String,
        file_path: String,
        description: Option<String>,
    },
    UntrustedAgentDefinitionUsed {
        role: String,
        path: String,
        canonical_path: String,
    },
    RateLimitStatusUpdated {
        role: String,
        status: crate::RateLimitStatus,
    },
    RunFinished {
        final_state: String,
    },
    /// See [`EventKind::HookFindingEmitted`].
    HookFindingEmitted {
        /// The [`crate::HookPoint::as_str`] the emitting hook fired at.
        point: String,
        source: String,
        severity: String,
        file: Option<String>,
        description: String,
        action: Option<String>,
    },
}

impl RunEvent {
    /// The [`EventKind`] this event's own variant corresponds to.
    pub fn kind(&self) -> EventKind {
        match self {
            RunEvent::RunStarted { .. } => EventKind::RunStarted,
            RunEvent::WorkflowResolved { .. } => EventKind::WorkflowResolved,
            RunEvent::CycleStarted { .. } => EventKind::CycleStarted,
            RunEvent::AgentStarted { .. } => EventKind::AgentStarted,
            RunEvent::AgentProgress { .. } => EventKind::AgentProgress,
            RunEvent::AgentFinished { .. } => EventKind::AgentFinished,
            RunEvent::FindingRaised { .. } => EventKind::FindingRaised,
            RunEvent::EvidenceCaptured { .. } => EventKind::EvidenceCaptured,
            RunEvent::UntrustedAgentDefinitionUsed { .. } => {
                EventKind::UntrustedAgentDefinitionUsed
            }
            RunEvent::RateLimitStatusUpdated { .. } => EventKind::RateLimitStatusUpdated,
            RunEvent::RunFinished { .. } => EventKind::RunFinished,
            RunEvent::HookFindingEmitted { .. } => EventKind::HookFindingEmitted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventRecord {
    pub id: String,
    pub run_id: String,
    pub event: RunEvent,
    /// RFC3339 timestamp, same convention as every other `warden::db` timestamp column.
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndecodableReason {
    UnknownEventType,
    PayloadDeserialize,
    /// `payload_json` decoded to a valid [`RunEvent`], but its own kind ([`RunEvent::kind`])
    /// disagrees with the row's declared `event_type` column.
    KindMismatch {
        payload_kind: String,
    },
}

impl std::fmt::Display for UndecodableReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndecodableReason::UnknownEventType => write!(f, "unknown event_type"),
            UndecodableReason::PayloadDeserialize => {
                write!(f, "payload_json failed to deserialize")
            }
            UndecodableReason::KindMismatch { payload_kind } => {
                write!(f, "payload's own kind is {payload_kind:?}")
            }
        }
    }
}

/// One `events` row whose `payload_json`/`event_type` could not be turned into a [`RunEvent`] --
/// see [`UndecodableReason`] for why this happens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndecodableEvent {
    pub id: String,
    pub run_id: String,
    pub event_type: String,
    pub reason: UndecodableReason,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunEventHistoryEntry {
    /// A row that decoded and validated cleanly.
    Decoded(RunEventRecord),
    Undecodable(UndecodableEvent),
}

impl RunEventHistoryEntry {
    /// The row's own `id`, whichever variant this is -- used to key deduplication/ordering the same
    /// way a [`RunEventRecord`]'s `id` already does.
    pub fn id(&self) -> &str {
        match self {
            RunEventHistoryEntry::Decoded(record) => &record.id,
            RunEventHistoryEntry::Undecodable(event) => &event.id,
        }
    }

    /// The row's own `created_at`, whichever variant this is.
    pub fn created_at(&self) -> &str {
        match self {
            RunEventHistoryEntry::Decoded(record) => &record.created_at,
            RunEventHistoryEntry::Undecodable(event) => &event.created_at,
        }
    }

    /// The decoded [`RunEventRecord`], or `None` for an [`RunEventHistoryEntry::Undecodable`] row.
    pub fn decoded(&self) -> Option<&RunEventRecord> {
        match self {
            RunEventHistoryEntry::Decoded(record) => Some(record),
            RunEventHistoryEntry::Undecodable(_) => None,
        }
    }

    /// The decoded [`RunEvent`] itself, or `None` for an [`RunEventHistoryEntry::Undecodable`] row.
    pub fn event(&self) -> Option<&RunEvent> {
        self.decoded().map(|record| &record.event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_kind() -> Vec<EventKind> {
        vec![
            EventKind::RunStarted,
            EventKind::WorkflowResolved,
            EventKind::CycleStarted,
            EventKind::AgentStarted,
            EventKind::AgentProgress,
            EventKind::AgentFinished,
            EventKind::FindingRaised,
            EventKind::EvidenceCaptured,
            EventKind::UntrustedAgentDefinitionUsed,
            EventKind::RateLimitStatusUpdated,
            EventKind::RunFinished,
            EventKind::HookFindingEmitted,
        ]
    }

    #[test]
    fn event_kind_round_trips_through_its_string_form() {
        for kind in every_kind() {
            assert_eq!(EventKind::parse(kind.as_str()).unwrap(), kind);
        }
    }

    #[test]
    fn unknown_event_kind_string_is_a_typed_error_not_a_panic() {
        assert_eq!(
            EventKind::parse("bogus"),
            Err(CoreError::UnknownEventKind("bogus".to_string()))
        );
    }

    fn sample(kind: EventKind) -> RunEvent {
        match kind {
            EventKind::RunStarted => RunEvent::RunStarted {
                intent: "do the thing".to_string(),
                branch: "main".to_string(),
                max_cycles: 5,
            },
            EventKind::WorkflowResolved => RunEvent::WorkflowResolved {
                name: "quality-loop".to_string(),
                entry: 0,
                steps: vec![
                    WorkflowStepWire {
                        index: 0,
                        id: "implementation".to_string(),
                        kind: "agent".to_string(),
                        on_clean: "review".to_string(),
                        on_blocking: "implementation".to_string(),
                        on_error: "failed".to_string(),
                        max_cycles: None,
                        captures_evidence: false,
                    },
                    WorkflowStepWire {
                        index: 1,
                        id: "review".to_string(),
                        kind: "agent".to_string(),
                        on_clean: "converged".to_string(),
                        on_blocking: "implementation".to_string(),
                        on_error: "failed".to_string(),
                        max_cycles: Some(3),
                        captures_evidence: true,
                    },
                ],
            },
            EventKind::CycleStarted => RunEvent::CycleStarted { cycle_number: 1 },
            EventKind::AgentStarted => RunEvent::AgentStarted {
                role: "coder".to_string(),
            },
            EventKind::AgentProgress => RunEvent::AgentProgress {
                role: "coder".to_string(),
                detail: "running `cargo test`".to_string(),
            },
            EventKind::AgentFinished => RunEvent::AgentFinished {
                role: "coder".to_string(),
                exit_code: 0,
                usage: Some(crate::TokenUsage::new(100, 50, Some(10), None)),
            },
            EventKind::FindingRaised => RunEvent::FindingRaised {
                cycle_number: 1,
                source: "reviewer".to_string(),
                severity: "blocking".to_string(),
                file: Some("src/lib.rs".to_string()),
                description: "missing test".to_string(),
                action: Some("add one".to_string()),
            },
            EventKind::EvidenceCaptured => RunEvent::EvidenceCaptured {
                cycle_number: 1,
                evidence_type: "image".to_string(),
                file_path: ".warden/evidence/1/screenshot.png".to_string(),
                description: Some("login screen".to_string()),
            },
            EventKind::UntrustedAgentDefinitionUsed => RunEvent::UntrustedAgentDefinitionUsed {
                role: "reviewer".to_string(),
                path: "/repo/.warden/agents/reviewer.md".to_string(),
                canonical_path: "/repo/.warden/agents/reviewer.md".to_string(),
            },
            EventKind::RateLimitStatusUpdated => RunEvent::RateLimitStatusUpdated {
                role: "coder".to_string(),
                status: crate::RateLimitStatus::new(
                    crate::RateLimitState::AllowedWarning,
                    crate::RateLimitWindow::SevenDay,
                    0.93,
                    false,
                    0.75,
                    1785686400,
                ),
            },
            EventKind::RunFinished => RunEvent::RunFinished {
                final_state: "converged".to_string(),
            },
            EventKind::HookFindingEmitted => RunEvent::HookFindingEmitted {
                point: "before_push".to_string(),
                source: "warden".to_string(),
                severity: "blocking".to_string(),
                file: Some("src/lib.rs".to_string()),
                description: "AWS key in diff".to_string(),
                action: None,
            },
        }
    }

    #[test]
    fn every_variant_reports_its_own_kind() {
        for kind in every_kind() {
            assert_eq!(sample(kind).kind(), kind);
        }
    }

    #[test]
    fn run_event_round_trips_through_json() {
        for kind in every_kind() {
            let event = sample(kind);
            let json = serde_json::to_string(&event).unwrap();
            let decoded: RunEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, event);
        }
    }

    #[test]
    fn run_event_record_round_trips_through_json() {
        let record = RunEventRecord {
            id: "event-1".to_string(),
            run_id: "run-1".to_string(),
            event: RunEvent::RunFinished {
                final_state: "converged".to_string(),
            },
            created_at: "2026-07-12T00:00:00+00:00".to_string(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let decoded: RunEventRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn decoded_history_entry_exposes_its_record_and_event() {
        let record = RunEventRecord {
            id: "event-1".to_string(),
            run_id: "run-1".to_string(),
            event: RunEvent::CycleStarted { cycle_number: 1 },
            created_at: "2026-07-12T00:00:00+00:00".to_string(),
        };
        let entry = RunEventHistoryEntry::Decoded(record.clone());

        assert_eq!(entry.id(), "event-1");
        assert_eq!(entry.created_at(), "2026-07-12T00:00:00+00:00");
        assert_eq!(entry.decoded(), Some(&record));
        assert_eq!(entry.event(), Some(&record.event));
    }

    #[test]
    fn undecodable_history_entry_has_no_decoded_event() {
        let entry = RunEventHistoryEntry::Undecodable(UndecodableEvent {
            id: "event-2".to_string(),
            run_id: "run-1".to_string(),
            event_type: "run_finished".to_string(),
            reason: UndecodableReason::KindMismatch {
                payload_kind: "cycle_started".to_string(),
            },
            created_at: "2026-07-12T00:00:01+00:00".to_string(),
        });

        assert_eq!(entry.id(), "event-2");
        assert_eq!(entry.created_at(), "2026-07-12T00:00:01+00:00");
        assert_eq!(entry.decoded(), None);
        assert_eq!(entry.event(), None);
    }

    #[test]
    fn undecodable_reason_display_is_distinct_per_variant() {
        let unknown = UndecodableReason::UnknownEventType.to_string();
        let deserialize = UndecodableReason::PayloadDeserialize.to_string();
        let mismatch = UndecodableReason::KindMismatch {
            payload_kind: "cycle_started".to_string(),
        }
        .to_string();

        assert_ne!(unknown, deserialize);
        assert_ne!(deserialize, mismatch);
        assert_ne!(unknown, mismatch);
        assert!(mismatch.contains("cycle_started"), "{mismatch}");
    }

    #[test]
    fn undecodable_event_round_trips_through_json() {
        let event = UndecodableEvent {
            id: "event-2".to_string(),
            run_id: "run-1".to_string(),
            event_type: "run_finished".to_string(),
            reason: UndecodableReason::KindMismatch {
                payload_kind: "cycle_started".to_string(),
            },
            created_at: "2026-07-12T00:00:01+00:00".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: UndecodableEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn agent_finished_round_trips_with_no_usage_reported() {
        let event = RunEvent::AgentFinished {
            role: "coder".to_string(),
            exit_code: 0,
            usage: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: RunEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn agent_finished_decodes_from_a_pre_issue_53_payload_missing_the_usage_field() {
        let json = r#"{"kind":"agent_finished","role":"coder","exit_code":0}"#;
        let decoded: RunEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            decoded,
            RunEvent::AgentFinished {
                role: "coder".to_string(),
                exit_code: 0,
                usage: None,
            }
        );
    }

    #[test]
    fn workflow_resolved_kind_round_trips_through_its_string_form() {
        assert_eq!(
            EventKind::parse(EventKind::WorkflowResolved.as_str()).unwrap(),
            EventKind::WorkflowResolved
        );
        assert_eq!(EventKind::WorkflowResolved.as_str(), "workflow_resolved");
    }

    #[test]
    fn workflow_resolved_round_trips_through_json_with_every_field_intact() {
        let event = sample(EventKind::WorkflowResolved);
        let json = serde_json::to_string(&event).unwrap();
        let decoded: RunEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, event);
        assert_eq!(decoded.kind(), EventKind::WorkflowResolved);

        let RunEvent::WorkflowResolved { name, entry, steps } = decoded else {
            panic!("expected WorkflowResolved, got {decoded:?}");
        };
        assert_eq!(name, "quality-loop");
        assert_eq!(entry, 0);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].id, "review");
        assert_eq!(steps[1].on_clean, "converged");
        assert_eq!(steps[1].max_cycles, Some(3));
        assert!(steps[1].captures_evidence);
    }

    #[test]
    fn unknown_event_type_string_never_decodes_as_workflow_resolved() {
        assert_eq!(
            EventKind::parse("workflow_step_added"),
            Err(CoreError::UnknownEventKind(
                "workflow_step_added".to_string()
            ))
        );
    }

    /// The variant's whole point is to carry *data*, decoupled from `crate::workflow`'s own types:
    /// a reader that knows nothing about `StepTarget`, `Role` or `StepKind` -- an older
    /// `warden-tui`, or anything reading the `events` table with `jq` -- must still be able to walk
    /// every field. Pin the flat, string-keyed encoding: a transition is a bare string, never a
    /// nested enum tag like `{"Step":1}`, and no field is dropped.
    #[test]
    fn workflow_resolved_encodes_as_plain_json_readable_without_any_warden_type() {
        let json = serde_json::to_string(&sample(EventKind::WorkflowResolved)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["kind"], "workflow_resolved");
        assert!(value["name"].is_string());
        assert!(value["entry"].is_u64());

        let steps = value["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        for step in steps {
            let object = step.as_object().unwrap();
            let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                vec![
                    "captures_evidence",
                    "id",
                    "index",
                    "kind",
                    "max_cycles",
                    "on_blocking",
                    "on_clean",
                    "on_error",
                ],
                "the wire shape must stay flat and complete: {step}"
            );
            assert!(step["index"].is_u64());
            assert!(step["id"].is_string());
            assert!(step["kind"].is_string());
            assert!(step["captures_evidence"].is_boolean());
            for transition in ["on_clean", "on_blocking", "on_error"] {
                assert!(
                    step[transition].is_string(),
                    "{transition} must be a bare string, not a tagged enum: {step}"
                );
            }
            assert!(
                step["max_cycles"].is_u64() || step["max_cycles"].is_null(),
                "an absent step budget must be null, never an omitted key: {step}"
            );
        }

        assert_eq!(steps[0]["kind"], "agent");
        assert_eq!(steps[0]["max_cycles"], serde_json::Value::Null);
        assert_eq!(steps[1]["on_clean"], "converged");
        assert_eq!(steps[1]["max_cycles"], 3);
    }
}
