//! The dispatch table: one row per tracked issue, ordered running-first.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Row as TRow, Table, TableState};

use super::{fmt_ms, fmt_tokens};
use crate::model::Phase;
use crate::sched::Snapshot;

fn phase_style(phase: Phase, quarantined: bool) -> Style {
    if quarantined {
        return Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    }
    match phase {
        Phase::Running => Style::default().fg(Color::Green),
        Phase::RetryQueued => Style::default().fg(Color::Yellow),
        Phase::Queued => Style::default().fg(Color::Cyan),
        Phase::Quarantined => Style::default().fg(Color::Red),
        Phase::Released => Style::default().fg(Color::DarkGray),
    }
}

pub fn render(f: &mut Frame, area: Rect, snap: &Snapshot, selected: usize) {
    let header = TRow::new(
        ["ISSUE", "STATE", "PHASE", "ATT", "TURNS", "TOKENS", "AGE", "NOTE"]
            .into_iter()
            .map(|h| Cell::from(h).style(Style::default().fg(Color::DarkGray))),
    )
    .height(1);

    let rows: Vec<TRow> = snap
        .rows
        .iter()
        .map(|r| {
            let style = phase_style(r.phase, r.quarantined);

            // One "note" column, showing whichever of these is most worth knowing.
            let note = if r.quarantined {
                r.last_error.clone().unwrap_or_else(|| "quarantined".into())
            } else if let Some(due) = r.retry_in_ms {
                let held = if r.holds_slot { ", slot held" } else { "" };
                if due > 0 {
                    format!("retry in {}{held}", fmt_ms(due as u64))
                } else {
                    format!("retry due{held}")
                }
            } else if r.phase == Phase::Running {
                r.last_event.clone().unwrap_or_default()
            } else {
                r.title.clone()
            };

            TRow::new(vec![
                Cell::from(r.identifier.clone())
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Cell::from(r.tracker_state.clone()),
                Cell::from(r.phase.label()).style(style),
                Cell::from(if r.attempt > 0 { r.attempt.to_string() } else { "-".into() }),
                Cell::from(r.turns.to_string()),
                Cell::from(fmt_tokens(r.tokens)),
                Cell::from(if r.age_ms > 0 { fmt_ms(r.age_ms) } else { "-".into() }),
                Cell::from(note).style(Style::default().fg(if r.quarantined {
                    Color::Red
                } else {
                    Color::DarkGray
                })),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(14),
        Constraint::Length(11),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Length(13),
        Constraint::Length(8),
        Constraint::Min(16),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(" dispatch ", Style::default().add_modifier(Modifier::BOLD))),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("› ");

    let mut state = TableState::default();
    if !snap.rows.is_empty() {
        state.select(Some(selected.min(snap.rows.len() - 1)));
    }
    f.render_stateful_widget(table, area, &mut state);
}
