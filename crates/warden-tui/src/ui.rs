//! Rendering: projects a [`RunModel`] onto the screen. No business logic
//! here (code-standards.md, "TUI (ratatui)": "aucune logique métier dans le
//! code de rendu") -- every value shown is already-validated data the model
//! carries; this module only lays it out. The one exception worth naming is
//! [`render_evidence_pane`]'s dispatch through [`crate::evidence::render`]:
//! that's presentation logic (what to draw for a given rendering outcome),
//! not business logic (nothing here decides what evidence *means*).

use std::path::PathBuf;

use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, Paragraph};
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::Image;
use warden_core::{RunEvent, RunEventRecord, TokenUsage};

use crate::capabilities::GraphicsCapability;
use crate::evidence::{self, Evidence, EvidenceKind};
use crate::model::{AgentNode, CycleNode, NodeStatus, ReloopCause, RunModel};

/// Fixed height of the one-line run header.
const HEADER_HEIGHT: u16 = 3;

/// Fixed height reserved for the workflow tree pane (issue #54) -- like
/// [`EVIDENCE_PANE_HEIGHT`], not dynamically sized off its content: a fixed
/// budget keeps the rest of the layout (header/events/evidence) stable
/// across draws instead of jumping around as cycles come and go. A run with
/// more branches than fit simply scrolls off the bottom, exactly like the
/// events pane already does without an explicit scroll offset.
const TREE_PANE_HEIGHT: u16 = 12;

/// Fixed height reserved for the evidence pane when the model has evidence
/// to show. Not dynamically sized off the image itself: `ratatui_image`
/// fits whatever it's given into this area regardless (`Resize::Fit`), and
/// a fixed budget keeps the rest of the layout (header/events) stable
/// across draws instead of jumping around as evidence comes and goes.
const EVIDENCE_PANE_HEIGHT: u16 = 12;

/// Longest intent shown in the header before truncation (issue #54) --
/// keeps a pathologically long intent from crowding out the rest of the
/// header's single unwrapped line (run id, branch, status, live progress).
const MAX_HEADER_INTENT_LEN: usize = 60;

/// Draws the whole screen: a one-line run header, a scrollable event log,
/// and (only once the model has an `EvidenceCaptured` event to show) an
/// evidence pane below that -- inline via `capability`/`picker` when the
/// terminal supports it (ADR-0010), otherwise a fallback message.
///
/// Rebuilds the evidence rendering from scratch on every call rather than
/// caching it across frames: acceptable today since nothing in this
/// codebase yet produces `EvidenceCaptured` events at any real frequency
/// (Phase 7, issue #7, isn't implemented on this branch) -- worth revisiting
/// with a cache keyed on the evidence's file path if that changes.
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

/// Renders the evidence pane for `record` (an `EvidenceCaptured` event, per
/// [`RunModel::latest_evidence`]'s contract) into `area`: an inline image
/// when the terminal supports a graphics protocol and decoding/preparing it
/// succeeds, otherwise a one-line explanation of why not (ADR-0010's
/// universal fallback) -- never a panic or a blank pane on failure.
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
        // `RunModel::latest_evidence` only ever returns this variant; kept
        // as a graceful no-op rather than `unreachable!()` so a future
        // change to that contract fails soft here, not with a panic mid-draw.
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
            let status = if let Some(final_state) = model.final_state() {
                format!(
                    "finished: {final_state} -- run total {}",
                    format_token_usage(&model.total_token_usage())
                )
            } else {
                // Issue #43: separate per-phase budgets replace the single
                // "cycle N/max" the header used to show. Kept compact
                // (`review N, test N` rather than spelling out "budget"
                // twice) so it still leaves room for the live agent-progress
                // suffix below within a narrow terminal.
                // Issue #53: the run-wide running total, next to the
                // per-invocation figure each `AgentFinished` line in the
                // scrollable log already carries -- "n/a" (never `0`) until
                // at least one invocation has reported usage.
                let cycle_status = format!(
                    "cycle {} in progress (review {max_review_cycles}, test {max_test_cycles}) \
                     -- run total {}",
                    model.current_cycle_number(),
                    format_token_usage(&model.total_token_usage())
                );
                // Issue #33: what actually makes the header alive during a
                // long-running agent -- `None` before any progress has
                // arrived, or once `RunModel::current_progress`'s own
                // "stale after AgentFinished" rule clears it (see that
                // method's docs), in which case the plain cycle status
                // above is all there is to show.
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

/// Truncates `intent` to at most `max_len` characters, appending `"..."` when
/// it was cut (issue #54: "Header shows run intent/task ... truncation alone
/// is acceptable"). Operates on characters, not bytes, so a multi-byte UTF-8
/// intent is never sliced mid-codepoint.
fn truncate_intent(intent: &str, max_len: usize) -> String {
    if intent.chars().count() <= max_len {
        return intent.to_string();
    }
    let truncated: String = intent.chars().take(max_len.saturating_sub(3)).collect();
    format!("{truncated}...")
}

/// Renders `usage` (issue #53) as a compact, human-readable fragment --
/// "n/a" when `None` (a tool that reported no usage at all, never a
/// fabricated `0` -- see `warden_core::TokenUsage`'s own docs), otherwise
/// the grand total followed by its input/output/cache breakdown. No
/// business logic here beyond formatting already-validated data
/// (code-standards.md, "TUI (ratatui)": "aucune logique métier dans le code
/// de rendu") -- every number comes straight from the model's own
/// [`TokenUsage`].
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
    let items: Vec<ListItem> = model.events().iter().map(event_list_item).collect();
    List::new(items).block(Block::bordered().title(" events "))
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
        // Issue #33: dim/dark styling deliberately distinguishes a
        // declarative progress line (what the agent *reports* doing, per
        // this variant's own docs -- never a verified execution trace) from
        // every other, more consequential event kind in this list.
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
        // Issue #26: styled the same yellow as a "warning"-severity finding
        // -- this is exactly that in spirit, just about the *configuration*
        // of an independent role rather than its output.
        //
        // Issue #26 review, LOW: `path` alone (the literal,
        // pre-canonicalization path) is what an operator recognizes, but for
        // the degraded-user-config case (a coder-controlled
        // `XDG_CONFIG_HOME`, or a symlinked `<role>.md`) it doesn't
        // literally look like it's inside the repo/a worktree at all --
        // `canonical_path` is rendered too, whenever it actually differs, so
        // an operator sees where it really resolved to rather than just the
        // technically-true-but-unactionable literal path.
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
        RunEvent::RunFinished { final_state } => (
            Style::default().fg(Color::Green),
            format!("run finished: {final_state}"),
        ),
    };

    ListItem::new(Line::styled(format!("{} {text}", record.created_at), style))
}

/// Renders the run's workflow tree (issue #54) as a git-graph-like pane:
/// one branch per cycle, each carrying its agent-invocation nodes in order
/// plus (if it reboucled into another cycle) a visually distinct return
/// edge. The run itself is the implicit root -- already named in the header
/// -- so this pane starts directly at the first cycle's own branch.
fn workflow_tree_widget(model: &RunModel) -> List<'static> {
    let tree = model.workflow_tree();
    let items: Vec<ListItem> = if tree.cycles.is_empty() {
        vec![ListItem::new(Line::styled(
            "no cycle started yet",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        workflow_tree_lines(&tree.cycles)
    };
    List::new(items).block(Block::bordered().title(" workflow tree "))
}

/// Lays out every cycle's branch, its agent nodes, and its return edge (if
/// any) using git-graph rails (`│`, `├─`, `╰─`) -- pure formatting over
/// already-derived [`CycleNode`]s, no business logic (code-standards.md,
/// "TUI (ratatui)": "aucune logique métier dans le code de rendu").
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

        // Continuation rail for every line belonging to this branch: a
        // dangling "│" only when a later branch still follows.
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

        // Issue #54 acceptance criterion: reloops (coder<->reviewer, scoped
        // re-review, tester return) must be visually distinct -- rendered
        // as its own return-edge line, styled apart from a plain node.
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

/// Renders one agent-invocation node: role, clean/findings/failed/running
/// status, and tokens spent (issue #53) -- "n/a" via [`format_token_usage`]
/// when the tool reported none, or while the invocation is still running.
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

/// The human-readable label for a [`ReloopCause`] return edge -- names the
/// actual role-to-role path the orchestrator takes (`warden::orchestrator`'s
/// main loop), not just an abstract "reboucle".
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

    /// Issue #33: the whole point of this event -- while a cycle is still
    /// in progress, the header must show what the running agent last
    /// reported, not just the static "cycle N/M in progress" it showed
    /// before this issue.
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

        // Issue #53: the header's single (unwrapped) line now also carries a
        // "run total" token suffix ahead of this progress detail -- wide
        // enough that it doesn't get clipped off before reaching it.
        let backend = TestBackend::new(160, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &model, GraphicsCapability::None, None))
            .unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("coder: running cargo test"));
    }

    /// Once an agent has finished, the header must fall back to the plain
    /// cycle status rather than keep showing its now-stale last progress
    /// line (`RunModel::current_progress`'s own contract) -- the progress
    /// detail legitimately still appears once, as a historical entry in the
    /// scrollable event log below the header; what must disappear is the
    /// header's own *second*, "this is happening right now" repetition of
    /// it.
    #[test]
    fn draw_omits_stale_progress_from_the_header_after_the_agent_finishes() {
        // Issue #53: wide enough that the header's own single (unwrapped)
        // line -- now also carrying a "run total" token suffix -- isn't
        // clipped before its trailing progress detail, in either draw below.
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

    /// The scrollable event log must also carry each progress line, not
    /// only the header's "latest" summary.
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

    /// Issue #26: `UntrustedAgentDefinitionUsed` must actually reach the
    /// scrollable log, naming both the role and the path, not just be a
    /// match arm nothing ever exercises end-to-end. `path` and
    /// `canonical_path` agree here (the plain repo-convention case), so only
    /// one copy of the path should be rendered.
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

    /// Issue #26 review, LOW: for the degraded-user-config case, `path` (the
    /// literal, pre-canonicalization path an operator recognizes -- here
    /// looking like a perfectly ordinary user-config location) and
    /// `canonical_path` (where it actually resolves to, inside the repo)
    /// differ -- both must reach the rendered log line, not just the literal
    /// path, or an operator sees a technically-true-but-unactionable record.
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

    /// Acceptance criterion 3 (issue #8): the evidence pane must actually be
    /// reachable from `draw`, not dead code -- exercised here on a
    /// non-capable terminal (`GraphicsCapability::None`), which is the
    /// deterministic, no-real-terminal-needed branch of `evidence::render`'s
    /// dispatch (the inline-image branch is covered directly in
    /// `evidence.rs`'s own tests, which don't need a `Frame` at all).
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

    // -----------------------------------------------------------------
    // Token usage rendering (issue #53)
    // -----------------------------------------------------------------

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

        // Wide enough that the header's own single (unwrapped) line comfortably
        // fits the run total suffix alongside everything else it already
        // shows -- a `ratatui::Paragraph` without `.wrap(..)` clips rather
        // than wraps, so a too-narrow backend would silently truncate the
        // very suffix this test asserts on.
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

    // -----------------------------------------------------------------
    // Workflow tree pane (issue #54)
    // -----------------------------------------------------------------

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

    /// Acceptance criteria (issue #54): the tree pane shows a node per
    /// agent invocation (role + clean/findings status + tokens), and a
    /// visually-labeled return edge for a reviewer-driven reloop.
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

    /// Acceptance criteria (issue #54): a tester-driven reloop must render
    /// its own distinct return-edge label ("tester -> coder -> reviewer ->
    /// tester"), not be conflated with the reviewer-driven edge covered by
    /// the test above.
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

    /// Acceptance criteria (issue #54): a CI-driven reloop (issue
    /// #15/ADR-0011's `ChecksFailed` outcome, whose `FindingRaised` is
    /// attributed to the *next* cycle it seeds, per
    /// `RunModel::workflow_tree`'s own docs) must render its own distinct
    /// return-edge label on the *prior*, fully-clean cycle.
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
        // Cycle 1 itself is entirely clean -- it only reboucles because of
        // what CI reported after convergence, seeded into cycle 2.
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

    /// Degradation (issue #54): a node whose invocation never reported
    /// usage shows "n/a", never a fabricated `0`.
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

    // -----------------------------------------------------------------
    // Header intent truncation (issue #54)
    // -----------------------------------------------------------------

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
