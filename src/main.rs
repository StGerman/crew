//! symphony-cc entry point.
//!
//! Headless is the default; `--tui` opts into the dashboard. That asymmetry is deliberate —
//! it keeps the UI a client of the same snapshot an operator could curl, rather than a
//! privileged view that correctness quietly depends on. `--api` makes the curl literal: the
//! same published snapshot, over HTTP, with no second path to the store.
//!
//! The loop below is the one place that owns the `Scheduler`, which is why both operator
//! surfaces reach it the same way — a message on a channel, never a handle. The dashboard's
//! messages are fire-and-forget; the API's carry a `oneshot` to answer on, because an HTTP
//! client is owed a response and a keypress is not.
//!
//! `status` is the third surface and the odd one out: it is a *client* of a daemon in another
//! process, so it returns before any of the setup below. It opens no store, prepares no
//! worktree and needs no tracker credential — an operator asking what is running must not be
//! able to disturb what is running, and a second process touching `symphony.db` while the
//! daemon holds it would be exactly that.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use symphony_cc::api::client::{Client, endpoint};
use symphony_cc::api::mcp::OpsMcp;
use symphony_cc::api::{Api, Command, render};
use symphony_cc::broker::fake::FakeWrites;
use symphony_cc::broker::{self, Broker, BrokerLimits, TrackerWrites};
use symphony_cc::clock::{Clock, SystemClock};
use symphony_cc::config::Config;
use symphony_cc::project::{NoopProjector, Projector, TasksProjector, derive_session_id};
use symphony_cc::sched::{Scheduler, Snapshot};
use symphony_cc::store::Store;
use symphony_cc::tracker::Tracker;
use symphony_cc::tracker::fake::FakeTracker;
use symphony_cc::tracker::github::{GithubTracker, UreqHttp};
use symphony_cc::transcript::Transcripts;
use symphony_cc::tui::{Ui, UiAction};
use symphony_cc::worker::Worker;
use symphony_cc::worker::claude::{ClaudeWorker, DEFAULT_ENV_ALLOWLIST};
use symphony_cc::worker::fake::{FakeWorker, Script};
use symphony_cc::workspace::GitWorktreeWorkspace;
use tokio::sync::{mpsc, watch};

#[derive(Parser, Debug)]
#[command(name = "symphony-cc", about = "Tracker-driven orchestrator for coding agents")]
struct Args {
    /// Path to the TOML config. Global, so `status` can read `[api] bind` out of the same
    /// file the daemon was started with, written on either side of the subcommand.
    #[arg(short, long, default_value = "symphony.toml", global = true)]
    config: PathBuf,

    /// Show the terminal dashboard. Without it the service runs headless and logs.
    #[arg(long)]
    tui: bool,

    /// Stop after this many ticks. Useful for smoke tests in CI.
    #[arg(long)]
    max_ticks: Option<u64>,

    /// Serve the ops HTTP API on this address, overriding `[api]` in the config. A
    /// non-loopback address still needs `api.allow_public`. Under `status`, the address to
    /// query instead of the one to serve.
    #[arg(long, value_name = "ADDR", global = true)]
    api: Option<String>,

    /// Serve the ops API as MCP tools on this address, for the agent supervising this daemon,
    /// overriding `[api] mcp_bind`. Loopback unless `api.allow_public`. Never hand this
    /// address to a dispatched worker — see `api::mcp`.
    #[arg(long, value_name = "ADDR")]
    mcp: Option<String>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Print what a running daemon is doing, read from its ops API.
    Status(StatusArgs),
}

#[derive(clap::Args, Debug)]
struct StatusArgs {
    /// One issue in full, by dispatch id or tracker identifier. Omit for every issue.
    #[arg(value_name = "ISSUE")]
    issue: Option<String>,

    /// Print the API's JSON verbatim. For a script; the rendered form is for a person.
    #[arg(long)]
    json: bool,
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

    // Before the config is even loaded: `status` is a client, and a daemon-side preflight
    // failure is not its business to report.
    if let Some(Cmd::Status(status)) = &args.command {
        std::process::exit(run_status(&args, status));
    }

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

    let use_real_worker = cfg.worker.kind.trim().eq_ignore_ascii_case("claude");
    let use_github_tracker = cfg.tracker.kind.trim().eq_ignore_ascii_case("github");

    // Deliberately independent of the tracker: see WorkerConfig's doc for why a real tracker
    // does not imply a real worker.
    let worker: Arc<dyn Worker> = if use_real_worker {
        let bin = cfg.worker.bin.clone().unwrap_or_else(|| "claude".to_string());
        // An operator-supplied list replaces the default outright rather than extending it, so
        // what reaches the child is exactly what the config says.
        let env_allowlist = cfg
            .worker
            .env_allowlist
            .clone()
            .unwrap_or_else(|| DEFAULT_ENV_ALLOWLIST.iter().map(|s| s.to_string()).collect());
        Arc::new(ClaudeWorker::new(bin, env_allowlist, cfg.agent.max_turns_per_session))
    } else {
        let fake = Arc::new(FakeWorker::new(clock.clone()));
        // The demo scripts are keyed to FakeTracker::demo()'s own issue ids; pointless (and
        // silently ignored, since none of those ids would ever be dispatched) against a real
        // tracker's real ids.
        if !use_github_tracker {
            seed_demo_scripts(&fake);
        }
        fake
    };

    // One adapter, two traits: the GitHub tracker reads for the scheduler and writes for the
    // broker over the same credential, which never leaves this process either way.
    let (tracker, writes): (Arc<dyn Tracker>, Arc<dyn TrackerWrites>) = if use_github_tracker {
        let token = std::env::var("GITHUB_TOKEN")
            .context("GITHUB_TOKEN must be set when tracker.kind = \"github\"")?;
        let gh = Arc::new(GithubTracker::new(
            UreqHttp::default(),
            &cfg.tracker.owner,
            &cfg.tracker.repo,
            &token,
            &cfg.tracker.required_labels,
        ));
        (gh.clone(), gh)
    } else {
        // The demo tracker has nothing to write to, so broker calls are recorded and dropped.
        // That still exercises the whole path — scoping, budgets, audit — without a network.
        (Arc::new(FakeTracker::demo()), Arc::new(FakeWrites::new()))
    };

    // Best-effort, like the projector: a root that cannot be created costs post-mortems, not
    // dispatch. Defaults beside the worktrees rather than inside one — see `TranscriptsConfig`.
    let transcripts = if cfg.transcripts.enabled {
        let root = cfg.transcripts.root_in(&ws_root);
        match Transcripts::new(&root, cfg.transcripts.max_bytes_per_run, cfg.transcripts.keep_runs)
        {
            Ok(t) => {
                tracing::info!(root = %root.display(), keep = cfg.transcripts.keep_runs, "recording run transcripts");
                Some(t)
            }
            Err(e) => {
                tracing::warn!(root = %root.display(), error = %e, "transcript root unavailable; runs will leave no record on disk");
                None
            }
        }
    } else {
        tracing::info!("transcripts disabled by config; runs will leave no record on disk");
        None
    };

    let broker = if cfg.broker.enabled {
        start_broker(&cfg, writes, clock.clone())
    } else {
        tracing::info!("broker disabled by config; agents run without tracker tools");
        None
    };

    if use_real_worker && use_github_tracker {
        tracing::warn!(
            "real tracker + real worker: this run will dispatch actual coding agents against \
             real issues and let them commit to real worktrees"
        );
    }

    tracing::info!(
        config = %args.config.display(),
        db = %db_path.display(),
        workspaces = %ws_root.display(),
        tracker = %cfg.tracker.kind,
        limit = cfg.agent.max_concurrent,
        "starting"
    );

    let interval_ms = cfg.polling.interval_ms;

    // Read before the config moves into the scheduler; `--api` is an override of it, not a
    // second source of truth.
    let mut api_cfg = cfg.api.clone();
    if let Some(addr) = args.api.clone() {
        api_cfg.enabled = true;
        api_cfg.bind = addr;
    }
    if let Some(addr) = args.mcp.clone() {
        api_cfg.mcp_enabled = true;
        api_cfg.mcp_bind = addr;
    }

    let mut sched =
        Scheduler::new(cfg, clock.clone(), store, tracker, worker, workspace, projector);
    sched.set_broker(broker);
    sched.set_transcripts(transcripts);

    let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
    let (act_tx, mut act_rx) = mpsc::unbounded_channel::<UiAction>();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Command>();

    // Publish once before anything can observe, so the window between start and the first tick
    // shows the scheduler's real state rather than `Snapshot::default()` — whose `running 0/0`
    // reports a concurrency limit this process never had.
    let _ = snap_tx.send(sched.snapshot()?);

    // Best-effort by contract: the API failing to start costs the API. Dispatch is not the
    // scheduler's opinion of whether a port was free.
    if api_cfg.enabled {
        match symphony_cc::api::bind(&api_cfg).await {
            Ok(listener) => {
                tokio::spawn(Api::new(snap_rx.clone(), cmd_tx.clone()).serve(listener));
            }
            Err(e) => tracing::error!(error = %e, "ops API not started; scheduling continues"),
        }
    }

    // The same surface as tools, on its own listener. Same contract as the HTTP API above: a
    // bind failure costs this server, never a dispatch. It is served by the broker's transport
    // but is deliberately *not* the broker — nothing here passes it to `Broker`, and the
    // `--mcp-config` a worker receives is written by `Broker::open` alone, so no dispatched
    // agent learns this address. `a_dispatched_worker_is_not_handed_the_ops_tools` holds that.
    if api_cfg.mcp_enabled {
        match symphony_cc::api::mcp::bind(&api_cfg) {
            Ok(listener) => {
                let addr = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
                let ops = Arc::new(OpsMcp::new(Api::new(snap_rx.clone(), cmd_tx.clone())));
                broker::server::serve(ops, listener);
                tracing::info!(%addr, path = symphony_cc::api::mcp::PATH, "ops MCP server listening");
            }
            Err(e) => {
                tracing::error!(error = %e, "ops MCP server not started; scheduling continues")
            }
        }
    }

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
                        let cleared = sched.unquarantine(&id)?;
                        tracing::info!(issue_id = %id, cleared, "operator cleared quarantine");
                        let _ = snap_tx.send(sched.snapshot()?);
                    }
                }
            }
            Some(command) = cmd_rx.recv() => {
                // Every arm publishes before it answers, so a client that reads
                // `GET /snapshot` the instant its POST returns sees the effect it asked for.
                // A dropped reply channel is an ordinary outcome, not an error: it means the
                // client went away, and nothing here waits to find out.
                match command {
                    Command::Tick(reply) => {
                        if let Err(e) = sched.tick() { tracing::error!(error = %e, "api tick failed"); }
                        let snap = sched.snapshot();
                        if let Ok(s) = &snap { let _ = snap_tx.send(s.clone()); }
                        let _ = reply.send(snap);
                    }
                    Command::Unquarantine { issue_id, reply } => {
                        let cleared = sched.unquarantine(&issue_id);
                        if let Ok(c) = &cleared {
                            tracing::info!(issue_id = %issue_id, cleared = c, "api cleared quarantine");
                        }
                        if let Ok(s) = sched.snapshot() { let _ = snap_tx.send(s); }
                        let _ = reply.send(cleared);
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("interrupt received; shutting down");
                break;
            }
        }
    }

    // A RunHandle outlives the Scheduler unless something kills it explicitly; exiting with
    // runs still in flight would otherwise orphan real worker processes.
    if let Err(e) = sched.shutdown() {
        tracing::error!(error = %e, "shutdown cleanup failed");
    }

    if let Some(h) = ui {
        let _ = h.join();
    }
    Ok(())
}

/// `symphony-cc status`: ask a running daemon what it is doing, and say so plainly.
///
/// Returns a process exit code rather than a `Result`, because the two failures an operator
/// cares about are not the same event. "No daemon is listening" is the answer to a question a
/// script may legitimately be asking; rendering it through `anyhow` would bury a message
/// written to be read in a `Error:` chain written to be debugged.
///
/// `1` for anything that stopped this from printing a snapshot. The message on stderr is what
/// distinguishes the cases; see `StatusError`.
fn run_status(args: &Args, status: &StatusArgs) -> i32 {
    let client = Client::new(endpoint(args.api.as_deref(), &args.config));
    let addr = client.endpoint().addr.clone();

    let rendered = match (&status.issue, status.json) {
        (None, false) => client.snapshot().map(|s| render::snapshot(&s, &addr)),
        (Some(key), false) => client.issue(key).map(|r| render::issue(&r)),
        (None, true) => client.raw_snapshot(),
        (Some(key), true) => client.raw_issue(key),
    };

    match rendered {
        Ok(text) => {
            print!("{}", text);
            if !text.ends_with('\n') {
                println!();
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// Bind the broker's loopback listener and start serving.
///
/// Every failure here returns `None` rather than propagating: a broker that cannot start is an
/// agent without tracker tools, which is a degrade the whole design allows for. Failing startup
/// over it would turn an optional capability into a required one.
fn start_broker(
    cfg: &Config,
    writes: Arc<dyn TrackerWrites>,
    clock: Arc<dyn Clock>,
) -> Option<Arc<Broker>> {
    let listener = match broker::server::bind() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "broker could not bind; agents run without tracker tools");
            return None;
        }
    };
    let addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "broker listener has no address; running without tools");
            return None;
        }
    };

    // Per-process, so two orchestrators on one host cannot collide or read each other's tokens.
    let config_dir = std::env::temp_dir().join(format!("symphony-mcp-{}", std::process::id()));
    let limits = BrokerLimits {
        max_calls_per_run: cfg.broker.max_calls_per_run,
        max_calls_per_issue: cfg.broker.max_calls_per_issue,
    };

    let broker = match Broker::new(writes, clock, limits, cfg.known_states(), addr, &config_dir) {
        Ok(b) => Arc::new(b),
        Err(e) => {
            tracing::warn!(error = %e, "broker setup failed; agents run without tracker tools");
            return None;
        }
    };
    broker::server::serve(Arc::clone(&broker), listener);
    tracing::info!(%addr, states = ?cfg.known_states(), "tool broker listening");
    Some(broker)
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
