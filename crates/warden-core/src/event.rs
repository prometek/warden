//! Run events published on the Event Bus and persisted to `EVENTS` for replay
//! (Architecture.md §6, ADR-0008). Pure, in-memory types only -- the actual
//! socket transport and SQLite persistence are I/O and live in the `warden`
//! (publisher) and `warden-tui` (subscriber/replay reader) crates; this
//! module is only the shared wire/row shape both sides agree on, so a
//! payload written by one is never silently misread by the other.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

/// Discriminant for a [`RunEvent`], stored separately as `EVENTS.event_type`
/// so it can be filtered/indexed in SQL without deserializing every
/// `payload_json` row. Mirrors [`RunEvent`]'s own serde tag one-for-one --
/// [`RunEvent::kind`] and this module's tests keep the two in sync, the same
/// `as_str`/`parse` pattern [`crate::RunState`] and [`crate::AgentRole`]
/// already use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    RunStarted,
    CycleStarted,
    AgentStarted,
    /// Issue #33 / ADR-0008 amendment: a live-only, declarative progress
    /// signal, translated by the run's `warden::tool_adapter::ToolAdapter`
    /// from one line of an agent's streamed output. Unlike every other
    /// variant here, a [`RunEvent::AgentProgress`] is **never** persisted as
    /// an `events` row (`warden::orchestrator::Orchestrator` broadcasts it
    /// straight on the Event Bus, bypassing `db::insert_event` entirely) --
    /// this discriminant still exists so [`RunEvent::kind`] stays a total
    /// function, not because anything ever writes `"agent_progress"` to a
    /// `event_type` column.
    AgentProgress,
    AgentFinished,
    FindingRaised,
    /// Modeled now for forward compatibility with Phase 7 (Evidence Capture
    /// Adapter, issue #7) even though nothing in this codebase can produce
    /// one yet -- the `EVIDENCE` table Phase 7 introduces doesn't exist on
    /// this branch, so there is no data source to raise it from. Kept here
    /// rather than added later so the wire/row protocol doesn't need a
    /// breaking change once Phase 7 lands.
    EvidenceCaptured,
    /// Issue #26: a reviewer/tester definition for this run was resolved
    /// from the repo under review (`.warden/agents/<role>.md`) rather than
    /// the trusted user config directory -- only ever reachable with
    /// `--trust-repo-agents` and no user-config file for that role (see
    /// `warden::agent_def`'s own "Security: role-asymmetric resolution"
    /// docs). Published once per affected role, right after `RunStarted`,
    /// so the run's own permanent event log carries a record of which
    /// role(s) ran under a definition the coder can write to.
    UntrustedAgentDefinitionUsed,
    /// Issue #84: a run's `--tool` adapter reported a rate-limit/quota
    /// status for one invocation (`ToolAdapter::extract_rate_limit`) --
    /// published (and persisted as the run's last-known status) whenever
    /// that seam returns `Some`, never for a tool that reports nothing at
    /// all.
    RateLimitStatusUpdated,
    RunFinished,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::RunStarted => "run_started",
            EventKind::CycleStarted => "cycle_started",
            EventKind::AgentStarted => "agent_started",
            EventKind::AgentProgress => "agent_progress",
            EventKind::AgentFinished => "agent_finished",
            EventKind::FindingRaised => "finding_raised",
            EventKind::EvidenceCaptured => "evidence_captured",
            EventKind::UntrustedAgentDefinitionUsed => "untrusted_agent_definition_used",
            EventKind::RateLimitStatusUpdated => "rate_limit_status_updated",
            EventKind::RunFinished => "run_finished",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "run_started" => Ok(EventKind::RunStarted),
            "cycle_started" => Ok(EventKind::CycleStarted),
            "agent_started" => Ok(EventKind::AgentStarted),
            "agent_progress" => Ok(EventKind::AgentProgress),
            "agent_finished" => Ok(EventKind::AgentFinished),
            "finding_raised" => Ok(EventKind::FindingRaised),
            "evidence_captured" => Ok(EventKind::EvidenceCaptured),
            "untrusted_agent_definition_used" => Ok(EventKind::UntrustedAgentDefinitionUsed),
            "rate_limit_status_updated" => Ok(EventKind::RateLimitStatusUpdated),
            "run_finished" => Ok(EventKind::RunFinished),
            other => Err(CoreError::UnknownEventKind(other.to_string())),
        }
    }
}

/// A single structured event describing one significant transition of a run
/// (Architecture.md §5.4/§6). This is everything a `warden-tui` view needs
/// to render a live or replayed run -- the TUI never re-derives meaning from
/// raw agent output or ad-hoc SQL joins of its own (code-standards.md, "TUI
/// (ratatui)": "aucune logique métier dans le code de rendu").
///
/// `role`/`source`/`severity`/`final_state` are carried as their already-
/// validated `as_str()` string form (not the `warden_core` enums
/// themselves): those enums don't derive `Serialize`/`Deserialize`, and
/// round-tripping through their stable string form here is the same
/// boundary convention `warden::db` already uses for SQLite columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEvent {
    RunStarted {
        intent: String,
        branch: String,
        /// Issue #43: the run's two independent per-phase budgets (ADR-0014)
        /// -- replaces the single `max_cycles` this event used to carry.
        max_review_cycles: u32,
        max_test_cycles: u32,
    },
    CycleStarted {
        cycle_number: u32,
    },
    AgentStarted {
        role: String,
    },
    /// A single declarative progress signal reported by an agent while it is
    /// still running (issue #33): what the agent's own tool CLI says it is
    /// doing right now (a streamed assistant message, or a `tool_use`
    /// block), translated by that tool's own
    /// `warden::tool_adapter::ToolAdapter` impl -- this type carries no
    /// knowledge of any one CLI's wire format (e.g. `stream-json` never
    /// leaks past the adapter that produces `detail`).
    ///
    /// **Declarative, not verified**: this is what the agent *reports*
    /// itself doing, not a checked execution trace -- ADR-0009's evidence
    /// keeps that role, and this event must never be presented as one.
    ///
    /// **Live-only** (ADR-0008 amendment, issue #33): unlike every other
    /// variant in this enum, a value of this variant is *never* persisted to
    /// the `events` table -- see [`EventKind::AgentProgress`]'s own docs. A
    /// `warden-tui` that attaches after the fact never replays it; it is
    /// only ever seen by a subscriber watching the run live at the moment it
    /// was published, exactly like the bus already tolerates losing events
    /// for a slow subscriber (`warden::event_bus`).
    AgentProgress {
        role: String,
        detail: String,
    },
    AgentFinished {
        role: String,
        exit_code: i32,
        /// Issue #53: what this invocation's tool CLI reported spending, via
        /// `warden::tool_adapter::ToolAdapter::extract_usage` -- `None` for a
        /// tool that reports no usage at all (rendered "n/a", never `0`; see
        /// `crate::TokenUsage`'s own docs), and for every event persisted
        /// before this field existed (`#[serde(default)]`, decoded as `None`
        /// rather than a hard deserialize error on old `events` rows).
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
    /// See [`EventKind::EvidenceCaptured`]: modeled, not yet produced by
    /// anything in this codebase (Phase 7, issue #7).
    EvidenceCaptured {
        cycle_number: u32,
        evidence_type: String,
        file_path: String,
        description: Option<String>,
    },
    /// See [`EventKind::UntrustedAgentDefinitionUsed`]'s own docs. `role` is
    /// always `"reviewer"` or `"tester"` (`AgentRole::as_str`) -- the coder
    /// is never subject to this (`warden::agent_def`'s own docs). `path` is
    /// the literal, pre-canonicalization path that was actually read (what
    /// an operator recognizes -- `.warden/agents/reviewer.md`, or the
    /// would-be user-config path), exactly as `Path::display` renders it.
    ///
    /// `canonical_path` (issue #26 review, LOW) is what `path` actually
    /// canonicalizes to (symlinks resolved) -- carried *alongside* `path`,
    /// never instead of it. For the plain repo-convention case the two
    /// usually agree; for the degraded-user-config case (a coder-controlled
    /// `XDG_CONFIG_HOME`, or a symlinked `<role>.md`) `path` may not
    /// literally look like it is inside the repo/a worktree at all -- e.g.
    /// `~/.config/warden/agents/reviewer.md` -- while `canonical_path` names
    /// exactly where it actually resolved to. Without this, an operator
    /// replaying the event for exactly the adversarial case this record
    /// exists for sees a path that is technically true but unactionable.
    UntrustedAgentDefinitionUsed {
        role: String,
        path: String,
        canonical_path: String,
    },
    /// Issue #84: the rate-limit/quota status one invocation's `--tool`
    /// adapter reported, via `warden::tool_adapter::ToolAdapter::extract_rate_limit`
    /// -- published only when that seam returns `Some` (a tool that reports
    /// nothing never produces this event at all, rather than one carrying a
    /// fabricated/empty status). `role` is whichever role's invocation
    /// reported it, the same convention as `AgentFinished::role`.
    RateLimitStatusUpdated {
        role: String,
        status: crate::RateLimitStatus,
    },
    RunFinished {
        final_state: String,
    },
}

impl RunEvent {
    /// The [`EventKind`] this event's own variant corresponds to. Kept as an
    /// explicit method (rather than relying solely on the serde tag) so
    /// callers that need the discriminant without going through JSON --
    /// e.g. `warden::db::insert_event` picking `EVENTS.event_type` -- have a
    /// single, testable source of truth for the mapping.
    pub fn kind(&self) -> EventKind {
        match self {
            RunEvent::RunStarted { .. } => EventKind::RunStarted,
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
        }
    }
}

/// One persisted/published event, together with the identity/ordering
/// metadata needed to store it as an `EVENTS` row *and* to deduplicate it on
/// the wire: `warden-tui` subscribes to the live socket **before** querying
/// SQLite for history (to avoid the gap a subscribe-after-query order would
/// risk), so it needs `id` to recognize a live event that's also about to
/// show up in the history query and skip replaying it twice (see
/// Architecture.md §5.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventRecord {
    pub id: String,
    pub run_id: String,
    pub event: RunEvent,
    /// RFC3339 timestamp, same convention as every other `warden::db`
    /// timestamp column.
    pub created_at: String,
}

/// Why one `events` row couldn't be decoded (issue #58) -- kept as a typed
/// enum rather than prose so a caller/UI can distinguish "this binary
/// predates this event kind" ([`Self::UnknownEventType`]) from "this row is
/// corrupted" ([`Self::PayloadDeserialize`]/[`Self::KindMismatch`]) without
/// parsing a message (code-standards.md: "erreurs typées explicitement,
/// jamais des strings").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndecodableReason {
    /// The row's `event_type` column doesn't match any [`EventKind`] this
    /// binary knows about ([`EventKind::parse`] failed) -- typically an
    /// older reader running against a database a newer `warden` wrote,
    /// after it started emitting an event kind this binary predates. The
    /// raw column value itself is already carried by
    /// [`UndecodableEvent::event_type`], so it isn't duplicated here.
    UnknownEventType,
    /// `payload_json` isn't valid JSON, or doesn't match the shape of any
    /// known [`RunEvent`] variant -- e.g. an event-payload reshape that
    /// shipped without a migration rewriting already-persisted rows (issue
    /// #43's `RunStarted`, issue #26's `UntrustedAgentDefinitionUsed`).
    PayloadDeserialize,
    /// `payload_json` decoded to a valid [`RunEvent`], but its own kind
    /// ([`RunEvent::kind`]) disagrees with the row's declared `event_type`
    /// column -- a corrupted row, or one written by something other than
    /// `db::insert_event`. `payload_kind` is [`EventKind::as_str`]'s stable
    /// string form (owned, rather than `&'static str`, so this type stays
    /// `Deserialize` -- needed to round-trip through the headless NDJSON
    /// dump, `warden_tui::main::run_headless`).
    KindMismatch { payload_kind: String },
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

/// One `events` row whose `payload_json`/`event_type` could not be turned
/// into a [`RunEvent`] (issue #58) -- see [`UndecodableReason`] for why this
/// happens. This is a real, if rare, possibility this codebase's migrations
/// don't rule out today: nothing rewrites already-persisted rows when an
/// event-payload reshape lands (e.g. issue #43's `RunStarted`, issue #26's
/// `UntrustedAgentDefinitionUsed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndecodableEvent {
    pub id: String,
    pub run_id: String,
    pub event_type: String,
    pub reason: UndecodableReason,
    pub created_at: String,
}

/// One row of `list_events_for_run`'s history (issue #58): a query that hit
/// a row it can't fully decode must still return every other row in the
/// run's history, with the bad row surfaced as an explicit
/// [`UndecodableEvent`] rather than either being silently dropped or failing
/// the whole query outright (code-standards.md: "no silent fallback, no
/// symptom-masking guards"). Both `warden::db::list_events_for_run` and
/// `warden_tui::db::list_events_for_run` (independently re-implemented, see
/// that module's own docs) produce this as their per-row outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEventHistoryEntry {
    /// A row that decoded and validated cleanly.
    Decoded(RunEventRecord),
    /// A row that could not be decoded/validated -- see
    /// [`UndecodableEvent`]'s own docs for why this happens.
    Undecodable(UndecodableEvent),
}

impl RunEventHistoryEntry {
    /// The row's own `id`, whichever variant this is -- used to key
    /// deduplication/ordering the same way a [`RunEventRecord`]'s `id`
    /// already does.
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

    /// The decoded [`RunEventRecord`], or `None` for an
    /// [`RunEventHistoryEntry::Undecodable`] row.
    pub fn decoded(&self) -> Option<&RunEventRecord> {
        match self {
            RunEventHistoryEntry::Decoded(record) => Some(record),
            RunEventHistoryEntry::Undecodable(_) => None,
        }
    }

    /// The decoded [`RunEvent`] itself, or `None` for an
    /// [`RunEventHistoryEntry::Undecodable`] row -- a shorthand for callers
    /// that only care about the event, not the surrounding row metadata.
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
            EventKind::CycleStarted,
            EventKind::AgentStarted,
            EventKind::AgentProgress,
            EventKind::AgentFinished,
            EventKind::FindingRaised,
            EventKind::EvidenceCaptured,
            EventKind::UntrustedAgentDefinitionUsed,
            EventKind::RateLimitStatusUpdated,
            EventKind::RunFinished,
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
                max_review_cycles: 5,
                max_test_cycles: 5,
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

    /// Issue #58: a [`RunEventHistoryEntry::Decoded`] row exposes its
    /// `RunEventRecord`/`RunEvent` through the shared accessors, and never
    /// through [`RunEventHistoryEntry::Undecodable`]'s own field.
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

    /// Issue #58: an [`RunEventHistoryEntry::Undecodable`] row still exposes
    /// its `id`/`created_at` (needed for ordering/dedup) but has no decoded
    /// `RunEvent` to hand back.
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

    /// [`UndecodableReason`]'s `Display` impl is what
    /// `warden_tui::ui::undecodable_list_item` renders -- must produce a
    /// distinct, non-empty string per variant so an operator can tell the
    /// three causes apart at a glance.
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

    /// Issue #58: `UndecodableEvent` must round-trip through JSON -- the
    /// headless NDJSON dump (`warden_tui::main::run_headless`) serializes it
    /// directly onto stdout.
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

    /// Issue #53: a tool that reports no usage at all yields `usage: None`
    /// on `AgentFinished` -- must round-trip cleanly, not be coerced into a
    /// zeroed [`crate::TokenUsage`].
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

    /// A pre-issue-#53 `events` row has no `usage` key in its `payload_json`
    /// at all -- `#[serde(default)]` must decode that as `None`, not fail
    /// the whole row.
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
}
