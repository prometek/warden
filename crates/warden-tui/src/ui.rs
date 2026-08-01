//! Rendering: projects a [`RunModel`] onto the screen.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, Paragraph};
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::Image;
use warden_core::{RunEvent, RunEventRecord, TokenUsage, UndecodableEvent};

use crate::capabilities::GraphicsCapability;
use crate::evidence::{self, Evidence, EvidenceKind};
use crate::model::{AgentNode, CycleNode, HistoryItem, NodeStatus, ReloopCause, RunModel};

/// Fixed height of the one-line run header.
const HEADER_HEIGHT: u16 = 3;

const TREE_PANE_HEIGHT: u16 = 12;

/// Fixed height reserved for the evidence pane when the model has evidence to show.
const EVIDENCE_PANE_HEIGHT: u16 = 12;

/// Longest intent shown in the header before truncation.
const MAX_HEADER_INTENT_LEN: usize = 60;

pub fn draw(
    frame: &mut Frame,
    model: &RunModel,
    capability: GraphicsCapability,
    picker: Option<&Picker>,
) {
    let area = frame.area();
    let latest_evidence = model.latest_evidence();

    if let Some(evidence_record) = latest_evidence {
        let [header_area, tree_area, events_area, evidence_area] = Layout::vertical([
            Constraint::Length(HEADER_HEIGHT),
            Constraint::Length(TREE_PANE_HEIGHT),
            Constraint::Min(0),
            Constraint::Length(EVIDENCE_PANE_HEIGHT),
        ])
        .areas(area);

        frame.render_widget(header_widget(model), header_area);
        frame.render_widget(workflow_tree_widget(model), tree_area);
        frame.render_widget(events_widget(model), events_area);
        render_evidence_pane(frame, evidence_record, capability, picker, evidence_area);
    } else {
        let [header_area, tree_area, events_area] = Layout::vertical([
            Constraint::Length(HEADER_HEIGHT),
            Constraint::Length(TREE_PANE_HEIGHT),
            Constraint::Min(0),
        ])
        .areas(area);

        frame.render_widget(header_widget(model), header_area);
        frame.render_widget(workflow_tree_widget(model), tree_area);
        frame.render_widget(events_widget(model), events_area);
    }
}

fn render_evidence_pane(
    frame: &mut Frame,
    record: &RunEventRecord,
    capability: GraphicsCapability,
    picker: Option<&Picker>,
    area: Rect,
) {
    let RunEvent::EvidenceCaptured {
        evidence_type,
        file_path,
        description,
        ..
    } = &record.event
    else {
        return;
    };

    let title = match description {
        Some(description) => format!(" evidence: {description} "),
        None => " evidence ".to_string(),
    };
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let evidence = Evidence {
        kind: EvidenceKind::parse(evidence_type),
        file_path: PathBuf::from(file_path),
        description: description.clone(),
    };
    let size = Size::new(inner.width, inner.height);

    match evidence::render(&evidence, capability, picker, size) {
        Ok(evidence::Rendering::Inline(protocol)) => {
            frame.render_widget(Image::new(&protocol), inner);
        }
        Ok(evidence::Rendering::ExternalViewer { path, reason }) => {
            frame.render_widget(
                Paragraph::new(format!("{reason}\nopen externally: {}", path.display())),
                inner,
            );
        }
        Err(error) => {
            frame.render_widget(
                Paragraph::new(format!("evidence unavailable: {error}")),
                inner,
            );
        }
    }
}

fn header_widget(model: &RunModel) -> Paragraph<'static> {
    let text = match (model.run_id(), model.run_started()) {
        (Some(run_id), Some((intent, branch, max_review_cycles, max_test_cycles))) => {
            let status = if let Some(resets_at) = model.quota_suspension_resets_at() {
                format!(
                    "SUSPENDED for quota (not failed) -- resumes {} -- run total {}",
                    format_reset_time(resets_at),
                    format_token_usage(&model.total_token_usage()),
                )
            } else if let Some(final_state) = model.final_state() {
                format!(
                    "finished: {final_state} -- run total {}",
                    format_token_usage(&model.total_token_usage())
                )
            } else {
                // separate per-phase budgets replace the single "cycle N/max" the header used to
                // show.
                let cycle_status = format!(
                    "cycle {} in progress (review {max_review_cycles}, test {max_test_cycles}) \
                     -- run total {}",
                    model.current_cycle_number(),
                    format_token_usage(&model.total_token_usage())
                );
                match model.current_progress() {
                    Some((role, detail)) => format!("{cycle_status} -- {role}: {detail}"),
                    None => cycle_status,
                }
            };
            format!(
                "run {run_id} [{branch}] \"{}\" -- {status}",
                truncate_intent(intent, MAX_HEADER_INTENT_LEN)
            )
        }
        _ => "waiting for run history...".to_string(),
    };

    Paragraph::new(text)
        .block(Block::bordered().title(" warden-tui (read-only) -- press q to quit "))
}

/// Formats one observed quota snapshot.
fn format_quota_status(status: Option<&warden_core::RateLimitStatus>) -> String {
    let Some(status) = status else {
        return "quota: n/a".to_string();
    };
    let remaining = (1.0 - status.utilization).max(0.0);
    format!(
        "quota: used {:.0}%, remaining {:.0}%, resets {}",
        status.utilization * 100.0,
        remaining * 100.0,
        format_reset_time(status.resets_at)
    )
}

/// UTC wall-clock rendering of the CLI's Unix reset timestamp.
fn format_reset_time(resets_at: i64) -> String {
    DateTime::<Utc>::from_timestamp(resets_at, 0)
        .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| format!("unix {resets_at}"))
}

/// Truncates `intent` to at most `max_len` characters, appending `"..."` when it was cut.
fn truncate_intent(intent: &str, max_len: usize) -> String {
    if intent.chars().count() <= max_len {
        return intent.to_string();
    }
    let truncated: String = intent.chars().take(max_len.saturating_sub(3)).collect();
    format!("{truncated}...")
}

fn format_token_usage(usage: &Option<TokenUsage>) -> String {
    let Some(usage) = usage else {
        return "tokens: n/a".to_string();
    };

    let mut parts = vec![
        format!("in {}", usage.input_tokens),
        format!("out {}", usage.output_tokens),
    ];
    if let Some(cache_read) = usage.cache_read_tokens {
        parts.push(format!("cache-read {cache_read}"));
    }
    if let Some(cache_creation) = usage.cache_creation_tokens {
        parts.push(format!("cache-write {cache_creation}"));
    }
    format!("tokens: {} ({})", usage.total(), parts.join(", "))
}

fn events_widget(model: &RunModel) -> List<'static> {
    let items: Vec<ListItem> = model
        .history()
        .into_iter()
        .map(|item| match item {
            HistoryItem::Event(record) => event_list_item(record),
            HistoryItem::Undecodable(event) => undecodable_list_item(event),
        })
        .collect();
    List::new(items).block(Block::bordered().title(" events "))
}

fn undecodable_list_item(event: &UndecodableEvent) -> ListItem<'static> {
    ListItem::new(Line::styled(
        format!(
            "{} event {} could not be decoded (event_type {:?}): {}",
            event.created_at, event.id, event.event_type, event.reason
        ),
        Style::default().fg(Color::Red),
    ))
}

fn event_list_item(record: &RunEventRecord) -> ListItem<'static> {
    let (style, text) = match &record.event {
        RunEvent::RunStarted {
            intent,
            branch,
            max_review_cycles,
            max_test_cycles,
        } => (
            Style::default().fg(Color::Cyan),
            format!(
                "run started: \"{intent}\" on {branch} (max {max_review_cycles} review cycles, \
                 max {max_test_cycles} test cycles)"
            ),
        ),
        RunEvent::CycleStarted { cycle_number } => (
            Style::default().fg(Color::Blue),
            format!("cycle {cycle_number} started"),
        ),
        RunEvent::AgentStarted { role } => {
            (Style::default().fg(Color::Gray), format!("{role} started"))
        }
        RunEvent::AgentProgress { role, detail } => (
            Style::default().fg(Color::DarkGray),
            format!("{role}: {detail}"),
        ),
        RunEvent::AgentFinished {
            role,
            exit_code,
            usage,
        } => (
            if *exit_code == 0 {
                Style::default().fg(Color::Gray)
            } else {
                Style::default().fg(Color::Red)
            },
            format!(
                "{role} finished (exit {exit_code}) -- {}",
                format_token_usage(usage)
            ),
        ),
        RunEvent::FindingRaised {
            severity,
            source,
            description,
            ..
        } => (
            match severity.as_str() {
                "blocking" => Style::default().fg(Color::Red),
                "warning" => Style::default().fg(Color::Yellow),
                _ => Style::default().fg(Color::White),
            },
            format!("[{severity}] {source}: {description}"),
        ),
        RunEvent::EvidenceCaptured {
            evidence_type,
            file_path,
            ..
        } => (
            Style::default().fg(Color::Magenta),
            format!("evidence captured ({evidence_type}): {file_path}"),
        ),
        RunEvent::UntrustedAgentDefinitionUsed {
            role,
            path,
            canonical_path,
        } => (
            Style::default().fg(Color::Yellow),
            if path == canonical_path {
                format!(
                    "{role} definition read from the repo under review (--trust-repo-agents): \
                     {path} -- untrusted, coder-controllable"
                )
            } else {
                format!(
                    "{role} definition read from the repo under review (--trust-repo-agents): \
                     {path} (resolves to {canonical_path}) -- untrusted, coder-controllable"
                )
            },
        ),
        RunEvent::RateLimitStatusUpdated { role, status } => (
            Style::default().fg(Color::Gray),
            format!(
                "{role}: rate limit status {} (used {:.0}%, remaining {:.0}%, resets {})",
                status.status.as_str(),
                status.utilization * 100.0,
                (1.0 - status.utilization).max(0.0) * 100.0,
                format_reset_time(status.resets_at),
            ),
        ),
        RunEvent::RunFinished { final_state } => (
            match warden_core::RunState::parse(final_state) {
                Ok(warden_core::RunState::AwaitingQuotaReset { resets_at: _ }) => {
                    Style::default().fg(Color::Yellow)
                }
                _ => Style::default().fg(Color::Green),
            },
            match warden_core::RunState::parse(final_state) {
                Ok(warden_core::RunState::AwaitingQuotaReset { resets_at }) => format!(
                    "run SUSPENDED for quota (not failed); resumes {}",
                    format_reset_time(resets_at)
                ),
                _ => format!("run finished: {final_state}"),
            },
        ),
    };

    ListItem::new(Line::styled(format!("{} {text}", record.created_at), style))
}

/// Renders the run's workflow tree as a git-graph-like pane.
fn workflow_tree_widget(model: &RunModel) -> List<'static> {
    let tree = model.workflow_tree();
    let mut items = vec![ListItem::new(Line::styled(
        format_quota_status(model.latest_rate_limit_status()),
        Style::default().fg(Color::Gray),
    ))];
    if tree.cycles.is_empty() {
        items.push(ListItem::new(Line::styled(
            "no cycle started yet",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        items.extend(workflow_tree_lines(&tree.cycles));
    }
    List::new(items).block(Block::bordered().title(" workflow tree "))
}

fn workflow_tree_lines(cycles: &[CycleNode]) -> Vec<ListItem<'static>> {
    let mut lines = Vec::new();
    let last_index = cycles.len() - 1;

    for (index, cycle) in cycles.iter().enumerate() {
        let is_last_cycle = index == last_index;
        let branch_glyph = if is_last_cycle { "╰─" } else { "├─" };
        lines.push(ListItem::new(Line::styled(
            format!("{branch_glyph}● cycle {}", cycle.cycle_number),
            Style::default().fg(Color::Blue),
        )));

        let rail = if is_last_cycle { "   " } else { "│  " };
        let agent_count = cycle.agents.len();
        for (agent_index, agent) in cycle.agents.iter().enumerate() {
            let is_last_agent = agent_index + 1 == agent_count && cycle.reloop.is_none();
            let agent_glyph = if is_last_agent {
                "╰──"
            } else {
                "├──"
            };
            lines.push(ListItem::new(agent_node_line(rail, agent_glyph, agent)));
        }

        // acceptance criterion: reloops must be visually distinct -- rendered as its own return-
        // edge line, styled apart from a plain node.
        if let Some(reloop) = cycle.reloop {
            lines.push(ListItem::new(Line::styled(
                format!("{rail}╰─↺ {}", reloop_description(reloop)),
                Style::default().fg(Color::Magenta),
            )));
        }

        if !is_last_cycle {
            lines.push(ListItem::new(Line::raw("│")));
        }
    }

    lines
}

/// Renders one agent-invocation node: role, clean/findings/failed/running status, and tokens spent.
fn agent_node_line(rail: &str, glyph: &str, agent: &AgentNode) -> Line<'static> {
    let (marker, label, style) = match agent.status {
        NodeStatus::Running => ("…", "running", Style::default().fg(Color::Yellow)),
        NodeStatus::Clean => ("✓", "clean", Style::default().fg(Color::Green)),
        NodeStatus::Findings => ("✗", "findings", Style::default().fg(Color::Red)),
        NodeStatus::Failed => ("!", "failed", Style::default().fg(Color::Red)),
    };
    let tokens = format_token_usage(&agent.tokens);
    Line::styled(
        format!(
            "{rail}{glyph} {:<9} {marker} {label:<9} {tokens}",
            agent.role
        ),
        style,
    )
}

/// The human-readable label for a [`ReloopCause`] return edge.
fn reloop_description(cause: ReloopCause) -> &'static str {
    match cause {
        ReloopCause::ReviewFinding => "reviewer -> coder (review reloop, scoped re-review next)",
        ReloopCause::TestFinding => "tester -> coder -> reviewer -> tester (test reloop)",
        ReloopCause::CiFailure => "CI checks failed -> coder (ci reloop)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn record(id: &str, event: RunEvent) -> RunEventRecord {
        RunEventRecord {
            id: id.to_string(),
            run_id: "run-1".to_string(),
            event,
            created_at: "2026-07-12T00:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn draw_with_no_events_shows_the_waiting_placeholder() {
        let model = RunModel::new();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("waiting for run history"));
    }

    #[test]
    fn draw_with_a_started_run_shows_its_intent_and_branch_in_the_header() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "add email validation".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 5,
                max_test_cycles: 5,
            },
        ));

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("add email validation"));
        assert!(content.contains("main"));
    }

    #[test]
    fn draw_shows_n_a_when_no_tool_reported_quota() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
            },
        ));

        let backend = TestBackend::new(180, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        assert!(buffer_to_string(terminal.backend().buffer()).contains("quota: n/a"));
    }

    #[test]
    fn draw_shows_quota_remaining_and_a_live_quota_suspension_as_not_failed() {
        let mut model = RunModel::new();
        let resets_at = 1_785_686_400;
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
            },
        ));
        model.apply(record(
            "e2",
            RunEvent::RateLimitStatusUpdated {
                role: "coder".to_string(),
                status: warden_core::RateLimitStatus::new(
                    warden_core::RateLimitState::AllowedWarning,
                    warden_core::RateLimitWindow::SevenDay,
                    0.93,
                    false,
                    0.75,
                    resets_at,
                ),
            },
        ));

        let render = |model: &RunModel| {
            let backend = TestBackend::new(240, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| draw(frame, model, GraphicsCapability::None, None))
                .unwrap();
            buffer_to_string(terminal.backend().buffer())
        };
        let quota_content = render(&model);
        assert!(
            quota_content.contains("quota: used 93%, remaining 7%"),
            "{quota_content}"
        );
        assert!(
            quota_content.contains("2026-08-02 16:00 UTC"),
            "{quota_content}"
        );

        model.apply(record(
            "e3",
            RunEvent::RunFinished {
                final_state: warden_core::RunState::AwaitingQuotaReset { resets_at }.as_str(),
            },
        ));
        let suspended_content = render(&model);
        assert!(
            suspended_content.contains("SUSPENDED for quota (not failed)"),
            "{suspended_content}"
        );
        assert!(
            suspended_content.contains("resumes 2026-08-02 16:00 UTC"),
            "{suspended_content}"
        );
    }

    #[test]
    fn draw_lists_every_applied_event() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
            },
        ));
        model.apply(record("e2", RunEvent::CycleStarted { cycle_number: 1 }));

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("run started"));
        assert!(content.contains("cycle 1 started"));
    }

    #[test]
    fn draw_lists_an_undecodable_history_row_in_its_chronological_place() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply_undecodable(warden_core::UndecodableEvent {
            id: "e2".to_string(),
            run_id: "run-1".to_string(),
            event_type: "run_finished".to_string(),
            reason: warden_core::UndecodableReason::KindMismatch {
                payload_kind: "cycle_started".to_string(),
            },
            created_at: "2026-07-12T00:00:01+00:00".to_string(),
        });
        model.apply(record(
            "e3",
            RunEvent::RunFinished {
                final_state: "converged".to_string(),
            },
        ));

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("cycle 1 started"), "{content}");
        assert!(
            content.contains("could not be decoded"),
            "the undecodable row must be rendered explicitly: {content}"
        );
        assert!(content.contains("run_finished"), "{content}");
        assert!(content.contains("run finished: converged"), "{content}");
    }

    #[test]
    fn draw_shows_the_current_agent_progress_in_the_header_while_a_cycle_is_in_progress() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
            },
        ));
        model.apply(record("e2", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e3",
            RunEvent::AgentProgress {
                role: "coder".to_string(),
                detail: "running cargo test".to_string(),
            },
        ));

        let backend = TestBackend::new(160, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("coder: running cargo test"));
    }

    #[test]
    fn draw_omits_stale_progress_from_the_header_after_the_agent_finishes() {
        let events_only = |model: &RunModel| {
            let backend = TestBackend::new(160, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| draw(frame, model, GraphicsCapability::None, None))
                .unwrap();
            buffer_to_string(terminal.backend().buffer())
                .matches("running cargo test")
                .count()
        };

        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
            },
        ));
        model.apply(record("e2", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e3",
            RunEvent::AgentProgress {
                role: "coder".to_string(),
                detail: "running cargo test".to_string(),
            },
        ));
        assert_eq!(
            events_only(&model),
            2,
            "before the agent finishes: once in the header, once in the event log"
        );

        model.apply(record(
            "e4",
            RunEvent::AgentFinished {
                role: "coder".to_string(),
                exit_code: 0,
                usage: None,
            },
        ));
        let content_after_finish = {
            let backend = TestBackend::new(160, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
                .unwrap();
            buffer_to_string(terminal.backend().buffer())
        };
        assert_eq!(
            content_after_finish.matches("running cargo test").count(),
            1,
            "after the agent finishes: only the historical event log entry remains, the \
             header's own repetition must be gone"
        );
        assert!(content_after_finish.contains("cycle 1 in progress (review 3, test 3)"));
    }

    #[test]
    fn draw_lists_agent_progress_events_in_the_scrollable_log() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::AgentProgress {
                role: "reviewer".to_string(),
                detail: "reviewing the diff".to_string(),
            },
        ));

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("reviewer: reviewing the diff"));
    }

    #[test]
    fn draw_lists_an_untrusted_agent_definition_used_event_naming_the_role_and_path() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::UntrustedAgentDefinitionUsed {
                role: "reviewer".to_string(),
                path: "/repo/.warden/agents/reviewer.md".to_string(),
                canonical_path: "/repo/.warden/agents/reviewer.md".to_string(),
            },
        ));

        let backend = TestBackend::new(220, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("reviewer"), "{content}");
        assert!(
            content.contains("/repo/.warden/agents/reviewer.md"),
            "{content}"
        );
        assert!(content.contains("untrusted"), "{content}");
    }

    #[test]
    fn draw_lists_an_untrusted_agent_definition_used_event_naming_both_the_literal_and_canonical_path(
    ) {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::UntrustedAgentDefinitionUsed {
                role: "reviewer".to_string(),
                path: "/home/dev/.config/warden/agents/reviewer.md".to_string(),
                canonical_path: "/repo/.warden/agents/reviewer.md".to_string(),
            },
        ));

        let backend = TestBackend::new(220, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(
            content.contains("/home/dev/.config/warden/agents/reviewer.md"),
            "{content}"
        );
        assert!(
            content.contains("/repo/.warden/agents/reviewer.md"),
            "{content}"
        );
    }

    #[test]
    fn draw_shows_an_evidence_pane_with_an_external_viewer_fallback_when_not_inline_capable() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::EvidenceCaptured {
                cycle_number: 1,
                evidence_type: "image".to_string(),
                file_path: "/tmp/screenshot.png".to_string(),
                description: Some("login screen".to_string()),
            },
        ));

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("evidence: login screen"));
        assert!(content.contains("/tmp/screenshot.png"));
    }

    #[test]
    fn draw_omits_the_evidence_pane_entirely_when_the_model_has_no_evidence() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(!content.contains("evidence"));
    }

    #[test]
    fn draw_shows_n_a_for_token_usage_before_any_agent_reports_it() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
            },
        ));
        model.apply(record("e2", RunEvent::CycleStarted { cycle_number: 1 }));

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("tokens: n/a"), "{content}");
    }

    #[test]
    fn draw_shows_the_token_breakdown_for_a_finished_agent_and_the_running_run_total() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
            },
        ));
        model.apply(record("e2", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e3",
            RunEvent::AgentFinished {
                role: "coder".to_string(),
                exit_code: 0,
                usage: Some(warden_core::TokenUsage::new(100, 50, Some(10), None)),
            },
        ));

        let backend = TestBackend::new(220, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(
            content
                .contains("coder finished (exit 0) -- tokens: 160 (in 100, out 50, cache-read 10)"),
            "{content}"
        );
        assert!(content.contains("run total tokens: 160"), "{content}");
    }

    #[test]
    fn draw_shows_a_placeholder_in_the_tree_pane_before_any_cycle_has_started() {
        let model = RunModel::new();
        let backend = TestBackend::new(100, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("no cycle started yet"), "{content}");
    }

    #[test]
    fn draw_shows_the_workflow_tree_with_node_status_tokens_and_a_review_reloop_edge() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
            },
        ));
        model.apply(record("e2", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e3",
            RunEvent::AgentStarted {
                role: "coder".to_string(),
            },
        ));
        model.apply(record(
            "e4",
            RunEvent::AgentFinished {
                role: "coder".to_string(),
                exit_code: 0,
                usage: Some(warden_core::TokenUsage::new(100, 50, None, None)),
            },
        ));
        model.apply(record(
            "e5",
            RunEvent::AgentStarted {
                role: "reviewer".to_string(),
            },
        ));
        model.apply(record(
            "e6",
            RunEvent::AgentFinished {
                role: "reviewer".to_string(),
                exit_code: 0,
                usage: None,
            },
        ));
        model.apply(record(
            "e7",
            RunEvent::FindingRaised {
                cycle_number: 1,
                source: "reviewer".to_string(),
                severity: "blocking".to_string(),
                file: None,
                description: "missing test".to_string(),
                action: None,
            },
        ));
        model.apply(record("e8", RunEvent::CycleStarted { cycle_number: 2 }));

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("workflow tree"), "{content}");
        assert!(content.contains("cycle 1"), "{content}");
        assert!(content.contains("coder"), "{content}");
        assert!(content.contains("clean"), "{content}");
        assert!(content.contains("reviewer"), "{content}");
        assert!(content.contains("findings"), "{content}");
        assert!(
            content.contains("tokens: 150 (in 100, out 50)"),
            "{content}"
        );
        assert!(
            content.contains("reviewer -> coder"),
            "the return edge must name the reviewer-driven reloop: {content}"
        );
    }

    #[test]
    fn draw_shows_the_workflow_tree_with_a_tester_driven_reloop_edge() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
            },
        ));
        model.apply(record("e2", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e3",
            RunEvent::AgentStarted {
                role: "coder".to_string(),
            },
        ));
        model.apply(record(
            "e4",
            RunEvent::AgentFinished {
                role: "coder".to_string(),
                exit_code: 0,
                usage: None,
            },
        ));
        model.apply(record(
            "e5",
            RunEvent::AgentStarted {
                role: "reviewer".to_string(),
            },
        ));
        model.apply(record(
            "e6",
            RunEvent::AgentFinished {
                role: "reviewer".to_string(),
                exit_code: 0,
                usage: None,
            },
        ));
        model.apply(record(
            "e7",
            RunEvent::AgentStarted {
                role: "tester".to_string(),
            },
        ));
        model.apply(record(
            "e8",
            RunEvent::AgentFinished {
                role: "tester".to_string(),
                exit_code: 0,
                usage: None,
            },
        ));
        model.apply(record(
            "e9",
            RunEvent::FindingRaised {
                cycle_number: 1,
                source: "tester".to_string(),
                severity: "blocking".to_string(),
                file: None,
                description: "flaky assertion".to_string(),
                action: None,
            },
        ));
        model.apply(record("e10", RunEvent::CycleStarted { cycle_number: 2 }));

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("cycle 1"), "{content}");
        assert!(content.contains("tester"), "{content}");
        assert!(content.contains("findings"), "{content}");
        assert!(
            content.contains("tester -> coder -> reviewer -> tester"),
            "the return edge must name the tester-driven reloop, not the review one: {content}"
        );
        assert!(
            !content.contains("reviewer -> coder"),
            "must not also render the review-reloop label: {content}"
        );
    }

    #[test]
    fn draw_shows_the_workflow_tree_with_a_ci_driven_reloop_edge() {
        let mut model = RunModel::new();
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
            },
        ));
        model.apply(record("e2", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e3",
            RunEvent::AgentStarted {
                role: "reviewer".to_string(),
            },
        ));
        model.apply(record(
            "e4",
            RunEvent::AgentFinished {
                role: "reviewer".to_string(),
                exit_code: 0,
                usage: None,
            },
        ));
        model.apply(record(
            "e5",
            RunEvent::AgentStarted {
                role: "tester".to_string(),
            },
        ));
        model.apply(record(
            "e6",
            RunEvent::AgentFinished {
                role: "tester".to_string(),
                exit_code: 0,
                usage: None,
            },
        ));
        model.apply(record("e7", RunEvent::CycleStarted { cycle_number: 2 }));
        model.apply(record(
            "e8",
            RunEvent::FindingRaised {
                cycle_number: 2,
                source: "ci".to_string(),
                severity: "blocking".to_string(),
                file: None,
                description: "checks failed".to_string(),
                action: None,
            },
        ));

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("cycle 1"), "{content}");
        assert!(
            content.contains("CI checks failed -> coder (ci reloop)"),
            "the return edge must name the ci-driven reloop distinctly: {content}"
        );
    }

    #[test]
    fn draw_shows_n_a_tokens_for_a_tree_node_that_reported_no_usage() {
        let mut model = RunModel::new();
        model.apply(record("e1", RunEvent::CycleStarted { cycle_number: 1 }));
        model.apply(record(
            "e2",
            RunEvent::AgentFinished {
                role: "coder".to_string(),
                exit_code: 0,
                usage: None,
            },
        ));

        let backend = TestBackend::new(100, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("tokens: n/a"), "{content}");
    }

    #[test]
    fn truncate_intent_leaves_a_short_intent_unchanged() {
        assert_eq!(
            truncate_intent("add email validation", 60),
            "add email validation"
        );
    }

    #[test]
    fn truncate_intent_cuts_a_long_intent_and_appends_an_ellipsis() {
        let intent = "a".repeat(100);
        let truncated = truncate_intent(&intent, 60);
        assert_eq!(truncated.chars().count(), 60);
        assert!(truncated.ends_with("..."), "{truncated}");
    }

    #[test]
    fn draw_truncates_a_very_long_intent_in_the_header() {
        let mut model = RunModel::new();
        let long_intent = "x".repeat(200);
        model.apply(record(
            "e1",
            RunEvent::RunStarted {
                intent: long_intent.clone(),
                branch: "main".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
            },
        ));

        let backend = TestBackend::new(220, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(
            !content.contains(&long_intent),
            "the full 200-char intent must not appear verbatim: {content}"
        );
        assert!(content.contains("..."), "{content}");
    }

    fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }
}
