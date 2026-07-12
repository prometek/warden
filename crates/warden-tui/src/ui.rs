//! Rendering: projects a [`RunModel`] onto the screen. No business logic
//! here (code-standards.md, "TUI (ratatui)": "aucune logique métier dans le
//! code de rendu") -- every value shown is already-validated data the model
//! carries; this module only lays it out.

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, Paragraph};
use ratatui::Frame;
use warden_core::{RunEvent, RunEventRecord};

use crate::model::RunModel;

/// Draws the whole screen: a one-line run header, and a scrollable event
/// log below it.
pub fn draw(frame: &mut Frame, model: &RunModel) {
    let area = frame.area();
    let [header_area, events_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);

    frame.render_widget(header_widget(model), header_area);
    frame.render_widget(events_widget(model), events_area);
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

        terminal.draw(|frame| draw(frame, &model)).unwrap();

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
        terminal.draw(|frame| draw(frame, &model)).unwrap();

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
        terminal.draw(|frame| draw(frame, &model)).unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("run started"));
        assert!(content.contains("cycle 1 started"));
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
