//! Terminal dashboard.
//!
//! Renders a [`Snapshot`] and nothing else. It has no handle on the store, the tracker or the
//! scheduler's internals, so it cannot become load-bearing: run headless and the system behaves
//! identically. Operator actions travel back as [`UiAction`] values on a channel rather than as
//! direct mutation.

mod detail;
mod dispatch;

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use tokio::sync::{mpsc, watch};

use crate::sched::Snapshot;
use crate::worker::TokenUsage;

/// Requests the dashboard sends back to the scheduler loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    ForceTick,
    Unquarantine(String),
    Quit,
}

pub struct Ui {
    snapshots: watch::Receiver<Snapshot>,
    actions: mpsc::UnboundedSender<UiAction>,
    selected: usize,
}

impl Ui {
    pub fn new(
        snapshots: watch::Receiver<Snapshot>,
        actions: mpsc::UnboundedSender<UiAction>,
    ) -> Self {
        Self { snapshots, actions, selected: 0 }
    }

    pub fn run(mut self) -> anyhow::Result<()> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen)?;
        let mut term = Terminal::new(CrosstermBackend::new(out))?;

        let result = self.event_loop(&mut term);

        // Restore the terminal even if the loop failed: leaving a user in raw mode on the
        // alternate screen is a far worse outcome than the error itself.
        disable_raw_mode().ok();
        execute!(term.backend_mut(), LeaveAlternateScreen).ok();
        term.show_cursor().ok();
        result
    }

    // Concrete in the backend: ratatui's `Backend::Error` carries no `Send + Sync` bound, so a
    // generic here cannot use `?` into `anyhow`, and one backend is all this ever needs.
    fn event_loop(
        &mut self,
        term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> anyhow::Result<()> {
        loop {
            let snap = self.snapshots.borrow().clone();
            let count = snap.rows.len();
            if count == 0 {
                self.selected = 0;
            } else if self.selected >= count {
                self.selected = count - 1;
            }
            term.draw(|f| self.draw(f, &snap))?;

            if !event::poll(Duration::from_millis(200))? {
                continue;
            }
            let Event::Key(KeyEvent { code, modifiers, kind, .. }) = event::read()? else {
                continue;
            };
            if kind != KeyEventKind::Press {
                continue; // Windows reports press and release; act once.
            }

            match (code, modifiers) {
                (KeyCode::Char('q') | KeyCode::Esc, _)
                | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    let _ = self.actions.send(UiAction::Quit);
                    return Ok(());
                }
                (KeyCode::Down | KeyCode::Char('j'), _) => {
                    self.selected = (self.selected + 1).min(count.saturating_sub(1));
                }
                (KeyCode::Up | KeyCode::Char('k'), _) => {
                    self.selected = self.selected.saturating_sub(1);
                }
                (KeyCode::Char('r'), _) => {
                    let _ = self.actions.send(UiAction::ForceTick);
                }
                (KeyCode::Char('u'), _) => {
                    if let Some(row) = snap.rows.get(self.selected).filter(|r| r.quarantined) {
                        let _ = self.actions.send(UiAction::Unquarantine(row.issue_id.clone()));
                    }
                }
                _ => {}
            }
        }
    }

    fn draw(&self, f: &mut Frame, snap: &Snapshot) {
        render_snapshot(f, snap, self.selected);
    }
}

/// Draw a snapshot. Free-standing so previews and tests can render without a live terminal or
/// a running scheduler — which is the same property that keeps the UI a pure function of the
/// snapshot rather than of hidden state.
pub fn render_snapshot(f: &mut Frame, snap: &Snapshot, selected: usize) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),    // dispatch table
            Constraint::Length(9), // detail
            Constraint::Length(3), // footer
        ])
        .split(f.area());

    let selected = selected.min(snap.rows.len().saturating_sub(1));
    dispatch::render(f, chunks[0], snap, selected);
    detail::render(f, chunks[1], snap.rows.get(selected));
    render_footer(f, chunks[2], snap);
}

fn render_footer(f: &mut Frame, area: Rect, snap: &Snapshot) {
    let saturated = snap.running >= snap.limit && snap.limit > 0;
    let mut spans = vec![
        Span::styled("running ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}/{}", snap.running, snap.limit),
            Style::default()
                .fg(if saturated { Color::Yellow } else { Color::Green })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   retry ", Style::default().fg(Color::DarkGray)),
        Span::styled(snap.retrying.to_string(), Style::default().fg(Color::Cyan)),
        Span::styled("   quarantined ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            snap.quarantined.to_string(),
            Style::default().fg(if snap.quarantined > 0 { Color::Red } else { Color::DarkGray }),
        ),
        Span::styled("   tokens ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            "{} in / {} out",
            fmt_count(snap.tokens.input),
            fmt_count(snap.tokens.output)
        )),
        Span::styled("   ticks ", Style::default().fg(Color::DarkGray)),
        Span::raw(snap.ticks.to_string()),
    ];

    // The sum covers only the runs that reported. Naming the ones that did not is what keeps it
    // from reading as a total when it is a lower bound.
    if snap.uncounted_runs > 0 {
        spans.insert(
            spans.len() - 2,
            Span::styled(
                format!(" (+{} uncounted)", snap.uncounted_runs),
                Style::default().fg(Color::Yellow),
            ),
        );
    }

    if let Some(err) = &snap.last_error {
        spans.push(Span::styled(
            format!("   ⚠ {err}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }

    let help = Span::styled(
        "  ↑↓ select · r tick · u unquarantine · q quit",
        Style::default().fg(Color::DarkGray),
    );

    let p = Paragraph::new(vec![Line::from(spans), Line::from(vec![help])])
        .block(Block::default().borders(Borders::TOP));
    f.render_widget(p, area);
}

/// Compact counts so a wide token column does not push the layout around.
/// `in/out` for a run that has reported, `-` for one that has not — the same mark the table
/// uses for an attempt count or age that does not exist yet. Never `0/0`: on this screen a zero
/// would say "free", and an unreported run is not free, it is unknown.
pub fn fmt_tokens(t: Option<TokenUsage>) -> String {
    match t {
        Some(t) => format!("{}/{}", fmt_count(t.input), fmt_count(t.output)),
        None => "-".to_string(),
    }
}

pub fn fmt_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

pub fn fmt_ms(ms: u64) -> String {
    let s = ms / 1_000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Phase;
    use crate::sched::Row;
    use ratatui::backend::TestBackend;

    fn row(identifier: &str, phase: Phase, quarantined: bool) -> Row {
        Row {
            issue_id: format!("iss-{identifier}"),
            identifier: identifier.into(),
            title: "some work".into(),
            tracker_state: "In Progress".into(),
            phase,
            attempt: 2,
            turns: 7,
            tokens: Some(TokenUsage { input: 1_200, output: 800 }),
            age_ms: 92_000,
            quarantined,
            last_error: quarantined.then(|| "agent exited unexpectedly".to_string()),
            workspace: Some("/tmp/ws/MT-1".into()),
            ..Default::default()
        }
    }

    /// Renders into an in-memory backend, so layout arithmetic and indexing are exercised
    /// without a terminal. Catches the panics that would otherwise only appear at runtime.
    fn render_to_string(snap: &Snapshot, selected: usize) -> String {
        let (tx, rx) = watch::channel(snap.clone());
        let (atx, _arx) = mpsc::unbounded_channel();
        let ui = Ui { snapshots: rx, actions: atx, selected };

        let mut term = Terminal::new(TestBackend::new(110, 26)).unwrap();
        term.draw(|f| ui.draw(f, snap)).unwrap();
        drop(tx);

        term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>()
    }

    #[test]
    fn an_empty_dashboard_renders_without_panicking() {
        let out = render_to_string(&Snapshot::default(), 0);
        assert!(out.contains("no issues tracked yet"));
    }

    #[test]
    fn rows_and_totals_reach_the_screen() {
        let snap = Snapshot {
            rows: vec![
                row("MT-601", Phase::Running, false),
                row("MT-602", Phase::RetryQueued, false),
                row("MT-603", Phase::Quarantined, true),
            ],
            running: 1,
            limit: 3,
            retrying: 1,
            quarantined: 1,
            tokens: TokenUsage { input: 12_400, output: 3_100 },
            uncounted_runs: 2,
            ticks: 9,
            ..Default::default()
        };
        let out = render_to_string(&snap, 0);

        for expect in
            ["MT-601", "MT-602", "MT-603", "running", "1/3", "12.4k", "+2 uncounted", "dispatch"]
        {
            assert!(out.contains(expect), "missing {expect:?} in rendered output");
        }
    }

    #[test]
    fn a_run_without_a_reported_total_shows_an_absence_not_a_zero() {
        // The table cell and the detail pane are two renderings of the same fact, and both must
        // say "unknown" rather than "free". The footer's own `running 0/0` counter is why this
        // does not simply forbid `0/0` in the whole frame.
        assert_eq!(fmt_tokens(None), "-");
        assert_eq!(fmt_tokens(Some(TokenUsage { input: 1_200, output: 80 })), "1.2k/80");

        let snap = Snapshot {
            rows: vec![Row { tokens: None, ..row("MT-601", Phase::Running, false) }],
            ..Default::default()
        };
        let out = render_to_string(&snap, 0);
        assert!(out.contains("not yet reported"), "the detail pane names the absence");
    }

    #[test]
    fn the_detail_pane_surfaces_a_quarantine_and_its_remedy() {
        let snap = Snapshot {
            rows: vec![row("MT-603", Phase::Quarantined, true)],
            quarantined: 1,
            ..Default::default()
        };
        let out = render_to_string(&snap, 0);
        assert!(out.contains("agent exited unexpectedly"));
        assert!(out.contains("press u to clear"));
    }

    #[test]
    fn a_selection_past_the_end_does_not_panic() {
        // The scheduler can shrink the row set between frames; the UI must tolerate it.
        let snap =
            Snapshot { rows: vec![row("MT-601", Phase::Running, false)], ..Default::default() };
        let out = render_to_string(&snap, 99);
        assert!(out.contains("MT-601"));
    }

    #[test]
    fn counts_are_abbreviated_at_each_magnitude() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_500), "1.5k");
        assert_eq!(fmt_count(2_400_000), "2.4M");
    }

    #[test]
    fn durations_read_naturally_at_each_scale() {
        assert_eq!(fmt_ms(0), "0s");
        assert_eq!(fmt_ms(45_000), "45s");
        assert_eq!(fmt_ms(90_000), "1m30s");
        assert_eq!(fmt_ms(7_380_000), "2h03m");
    }
}
