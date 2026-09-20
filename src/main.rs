//! symphony-cc entry point.
//!
//! Headless is the default; `--tui` opts into the dashboard. That asymmetry is deliberate —
//! it keeps the UI a client of the same snapshot an operator could curl, rather than a
//! privileged view that correctness quietly depends on.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use symphony_cc::clock::{Clock, SystemClock};
use symphony_cc::config::Config;
use symphony_cc::project::{NoopProjector, Projector, TasksProjector, derive_session_id};
use symphony_cc::sched::{Scheduler, Snapshot};
use symphony_cc::store::Store;
use symphony_cc::tracker::fake::FakeTracker;
use symphony_cc::tui::{Ui, UiAction};
use symphony_cc::worker::fake::{FakeWorker, Script};
use symphony_cc::workspace::GitWorktreeWorkspace;
use tokio::sync::{mpsc, watch};

#[derive(Parser, Debug)]
#[command(name = "symphony-cc", about = "Tracker-driven orchestrator for coding agents")]
struct Args {
    /// Path to the TOML config.
    #[arg(short, long, default_value = "symphony.toml")]
    config: PathBuf,

    /// Show the terminal dashboard. Without it the service runs headless and logs.
    #[arg(long)]
    tui: bool,

    /// Stop after this many ticks. Useful for smoke tests in CI.
    #[arg(long)]
    max_ticks: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // In TUI mode the alternate screen owns stdout, so logs go to stderr only.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "symphony_cc=info".into()),
        )
        .init();

    let cfg = Config::load(&args.config)
        .with_context(|| format!("loading config from {}", args.config.display()))?;

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());

    let db_path = std::env::var("SYMPHONY_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("symphony.db"));
    let store = Store::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    let ws_root = cfg
        .workspace
        .root
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("symphony_workspaces"));
    let repo = cfg.workspace.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    let workspace = Arc::new(GitWorktreeWorkspace::new(&ws_root, &repo)?);

    let tasks_root = std::env::var_os("SYMPHONY_TASKS_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude/tasks")))
        .unwrap_or_else(|| PathBuf::from(".claude/tasks"));
    // The session id is derived from the canonical workspace root rather than generated fresh,
    // so a restart of the same deployment updates one directory instead of littering a new one
    // on every process start.
    let session_id = derive_session_id(&workspace.root().display().to_string());
    let projector: Arc<dyn Projector> = match TasksProjector::new(&tasks_root, &session_id) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            tracing::warn!(error = %e, "task projection setup failed; running without it");
            Arc::new(NoopProjector)
        }
    };

    // Slice 1 is fakes end to end. The tracker and worker are still fakes — slices 3 and 4
    // swap these two lines for real implementations without the scheduler noticing.
    let tracker = Arc::new(FakeTracker::demo());
    let worker = Arc::new(FakeWorker::new(clock.clone()));
    seed_demo_scripts(&worker);

    tracing::info!(
        config = %args.config.display(),
        db = %db_path.display(),
        workspaces = %ws_root.display(),
        tracker = %cfg.tracker.kind,
        limit = cfg.agent.max_concurrent,
        "starting"
    );

    let interval_ms = cfg.polling.interval_ms;
    let mut sched =
        Scheduler::new(cfg, clock.clone(), store, tracker, worker, workspace, projector);

    let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
    let (act_tx, mut act_rx) = mpsc::unbounded_channel::<UiAction>();

    let ui = args.tui.then(|| {
        let rx = snap_rx.clone();
        let tx = act_tx.clone();
        std::thread::spawn(move || Ui::new(rx, tx).run())
    });

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // While the dashboard is up, refresh it faster than the poll interval so in-flight
    // progress animates. This drives rendering only; dispatch still happens on the tick.
    let mut repaint = tokio::time::interval(std::time::Duration::from_millis(250));

    let mut ticks = 0u64;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(e) = sched.tick() {
                    tracing::error!(error = %e, "tick failed");
                }
                ticks += 1;
                let _ = snap_tx.send(sched.snapshot()?);
                if args.max_ticks.is_some_and(|m| ticks >= m) {
                    tracing::info!(ticks, "max ticks reached; shutting down");
                    break;
                }
            }
            _ = repaint.tick(), if args.tui => {
                let _ = snap_tx.send(sched.snapshot()?);
            }
            Some(action) = act_rx.recv() => {
                match action {
                    UiAction::Quit => break,
                    UiAction::ForceTick => {
                        if let Err(e) = sched.tick() { tracing::error!(error = %e, "forced tick failed"); }
                        let _ = snap_tx.send(sched.snapshot()?);
                    }
                    UiAction::Unquarantine(id) => {
                        tracing::info!(issue_id = %id, "operator cleared quarantine");
                        sched.unquarantine(&id)?;
                        let _ = snap_tx.send(sched.snapshot()?);
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("interrupt received; shutting down");
                break;
            }
        }
    }

    if let Some(h) = ui {
        let _ = h.join();
    }
    Ok(())
}

/// Give the demo tracker a spread of behaviours so the dashboard shows every state worth
/// recognising: clean completions, work that continues, a hard failure, and a wedged agent.
fn seed_demo_scripts(w: &FakeWorker) {
    use symphony_cc::model::{ErrorClass, Outcome};

    w.set_default(Script::succeeds_in(12_000));
    w.script(
        "iss-002",
        Script::succeeds_in(20_000)
            .with_outcome(Outcome::Continue { why: "tests still failing".into() }),
    );
    w.script(
        "iss-003",
        Script::succeeds_in(9_000).with_outcome(Outcome::Failed {
            class: ErrorClass::AgentCrash,
            msg: "agent exited unexpectedly".into(),
        }),
    );
    w.script("iss-004", Script::succeeds_in(30_000));
    w.script(
        "iss-005",
        Script::succeeds_in(15_000)
            .with_outcome(Outcome::Blocked { why: "needs product decision".into() }),
    );
    w.script("iss-006", Script::stalls_after(6_000));
}
