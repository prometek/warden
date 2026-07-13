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
use warden_core::{RunEvent, RunEventRecord};

use crate::capabilities::GraphicsCapability;
use crate::evidence::{self, Evidence, EvidenceKind};
use crate::model::RunModel;

/// Fixed height reserved for the evidence pane when the model has evidence
/// to show. Not dynamically sized off the image itself: `ratatui_image`
/// fits whatever it's given into this area regardless (`Resize::Fit`), and
/// a fixed budget keeps the rest of the layout (header/events) stable
/// across draws instead of jumping around as evidence comes and goes.
const EVIDENCE_PANE_HEIGHT: u16 = 12;

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
        let [header_area, events_area, evidence_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(EVIDENCE_PANE_HEIGHT),
        ])
        .areas(area);

        frame.render_widget(header_widget(model), header_area);
        frame.render_widget(events_widget(model), events_area);
        render_evidence_pane(frame, evidence_record, capability, picker, evidence_area);
    } else {
        let [header_area, events_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);

        frame.render_widget(header_widget(model), header_area);
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
        (Some(run_id), Some((intent, branch, max_cycles))) => {
            let status = if let Some(final_state) = model.final_state() {
                format!("finished: {final_state}")
            } else {
                format!(
                    "cycle {}/{max_cycles} in progress",
                    model.current_cycle_number()
                )
            };
            format!("run {run_id} [{branch}] \"{intent}\" -- {status}")
        }
        _ => "waiting for run history...".to_string(),
    };

    Paragraph::new(text)
        .block(Block::bordered().title(" warden-tui (read-only) -- press q to quit "))
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
            max_cycles,
        } => (
            Style::default().fg(Color::Cyan),
            format!("run started: \"{intent}\" on {branch} (max {max_cycles} cycles)"),
        ),
        RunEvent::CycleStarted { cycle_number } => (
            Style::default().fg(Color::Blue),
            format!("cycle {cycle_number} started"),
        ),
        RunEvent::AgentStarted { role } => {
            (Style::default().fg(Color::Gray), format!("{role} started"))
        }
        RunEvent::AgentFinished { role, exit_code } => (
            if *exit_code == 0 {
                Style::default().fg(Color::Gray)
            } else {
                Style::default().fg(Color::Red)
            },
            format!("{role} finished (exit {exit_code})"),
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
        RunEvent::RunFinished { final_state } => (
            Style::default().fg(Color::Green),
            format!("run finished: {final_state}"),
        ),
    };

    ListItem::new(Line::styled(format!("{} {text}", record.created_at), style))
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
                max_cycles: 5,
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
                max_cycles: 3,
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
