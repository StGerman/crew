//! Render the dashboard to stdout without a terminal or a running scheduler.
//!
//! `cargo run --example dashboard_preview`
//!
//! Useful for reviewing layout changes in a diff, and as a standing reminder that the UI is a
//! pure function of a [`Snapshot`] — if it renders here, it renders anywhere.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use symphony_cc::model::Phase;
use symphony_cc::sched::{Row, Snapshot};
use symphony_cc::tui::render_snapshot;
use symphony_cc::worker::TokenUsage;

fn row(identifier: &str, title: &str, state: &str, phase: Phase) -> Row {
    Row {
        issue_id: format!("iss-{identifier}"),
        identifier: identifier.into(),
        title: title.into(),
        tracker_state: state.into(),
        phase,
        workspace: Some(format!("/tmp/symphony_workspaces/{identifier}-d36eac00286d")),
        url: Some(format!("https://tracker.example/issues/{identifier}")),
        ..Default::default()
    }
}

fn main() {
    let rows = vec![
        Row {
            turns: 7,
            tokens: Some(TokenUsage { input: 18_400, output: 6_200 }),
            age_ms: 214_000,
            ..row("MT-601", "Flaky retry on token refresh", "In Progress", Phase::Running)
        },
        Row {
            turns: 2,
            tokens: Some(TokenUsage { input: 4_100, output: 900 }),
            age_ms: 38_000,
            ..row("MT-604", "Document the webhook contract", "In Progress", Phase::Running)
        },
        Row {
            attempt: 1,
            turns: 4,
            tokens: Some(TokenUsage { input: 9_700, output: 2_300 }),
            age_ms: 92_000,
            ..row("MT-606", "Investigate slow cold start", "In Review", Phase::Running)
        },
        Row {
            attempt: 2,
            turns: 6,
            tokens: Some(TokenUsage { input: 11_200, output: 3_800 }),
            retry_in_ms: Some(38_000),
            last_error: Some("agent exited unexpectedly".into()),
            ..row("MT-603", "Migrate settings to new schema", "In Progress", Phase::RetryQueued)
        },
        Row {
            attempt: 3,
            turns: 12,
            tokens: Some(TokenUsage { input: 31_000, output: 9_400 }),
            quarantined: true,
            last_error: Some("template_render: unknown variable `issue.owner`".into()),
            ..row("MT-609", "Rewrite the importer", "In Progress", Phase::Quarantined)
        },
        Row {
            turns: 3,
            ..row("MT-605", "Drop the legacy export path", "In Progress", Phase::Released)
        },
    ];

    let snap = Snapshot {
        running: 3,
        limit: 3,
        retrying: 1,
        quarantined: 1,
        tokens: TokenUsage { input: 74_400, output: 22_600 },
        uncounted_runs: 1,
        ticks: 42,
        rows,
        ..Default::default()
    };

    let mut term = Terminal::new(TestBackend::new(108, 26)).unwrap();
    term.draw(|f| render_snapshot(f, &snap, 0)).unwrap();

    let buf = term.backend().buffer();
    for y in 0..buf.area.height {
        let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        println!("{}", line.trim_end());
    }
}
