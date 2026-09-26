//! Detail pane for the selected issue.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::{fmt_count, fmt_ms};
use crate::model::Phase;
use crate::sched::Row;

fn field<'a>(label: &'a str, value: impl Into<String>) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<12}"), Style::default().fg(Color::DarkGray)),
        Span::raw(value.into()),
    ])
}

pub fn render(f: &mut Frame, area: Rect, row: Option<&Row>) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(" detail ", Style::default().add_modifier(Modifier::BOLD)));

    let Some(r) = row else {
        let p = Paragraph::new("no issues tracked yet")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(p, area);
        return;
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                r.identifier.clone(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::raw(r.title.clone()),
        ]),
        field("state", format!("{}  ·  {}", r.tracker_state, r.phase.label())),
        field(
            "turns",
            match r.attempt {
                0 => format!("{} (first attempt)", r.turns),
                n => format!("{} (attempt {n})", r.turns),
            },
        ),
        field(
            "tokens",
            match r.tokens {
                Some(t) => format!("{} in / {} out", fmt_count(t.input), fmt_count(t.output)),
                None if r.phase == Phase::Running => "not yet reported".into(),
                None => "not reported".into(),
            },
        ),
    ];

    if let Some(run) = r.runs.first() {
        lines.push(field("model", run.model_label()));
    }
    if let Some(ws) = &r.workspace {
        lines.push(field("workspace", ws.clone()));
    }
    if let Some(t) = &r.transcript {
        lines.push(field("transcript", t.clone()));
    }
    if let Some(d) = &r.delivery {
        let pr = d.pr_url.clone().or_else(|| d.pr_number.map(|n| format!("#{n}")));
        lines.push(field(
            "delivery",
            format!(
                "{}{}  ·  rounds {}/{}",
                pr.map(|p| format!("{p}  ·  ")).unwrap_or_default(),
                d.stage,
                d.rounds_pr,
                d.rounds_issue
            ),
        ));
        if let Some(why) = d.handoff_reason.as_ref().or(d.review_error.as_ref()) {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<12}", "handed off"), Style::default().fg(Color::DarkGray)),
                Span::styled(why.clone(), Style::default().fg(Color::Yellow)),
            ]));
        }
    }
    if let Some(url) = &r.url {
        lines.push(field("url", url.clone()));
    }
    if let Some(due) = r.retry_in_ms {
        let when = if due > 0 { format!("in {}", fmt_ms(due as u64)) } else { "due now".into() };
        let held = if r.holds_slot { ", holding its slot" } else { "" };
        lines.push(field("retry", format!("{when}{held}")));
    }
    if let Some(err) = &r.last_error {
        lines.push(Line::from(vec![
            Span::styled(format!("{:<12}", "error"), Style::default().fg(Color::DarkGray)),
            Span::styled(err.clone(), Style::default().fg(Color::Red)),
        ]));
    }
    if r.quarantined {
        lines.push(Line::from(Span::styled(
            "quarantined — press u to clear",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }

    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: true }), area);
}
