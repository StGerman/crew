//! Scheduler behaviour, end to end, on a fake clock.
//!
//! Every test drives real `Scheduler::tick` against fake tracker/worker/workspace. Time only
//! moves when a test moves it, so there are no sleeps and nothing to flake. Each test names the
//! invariant it defends; several of them correspond directly to defects found in the spec.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use symphony_cc::broker::fake::FakeWrites;
use symphony_cc::broker::{Broker, BrokerLimits, TrackerWrites};
use symphony_cc::clock::{Clock, FakeClock};
use symphony_cc::config::{AgentConfig, Config, PollingConfig, TrackerConfig, WorkspaceConfig};
use symphony_cc::model::{ErrorClass, Issue, Outcome, Phase};
use symphony_cc::project::{NoopProjector, Projector, TasksProjector};
use symphony_cc::sched::Scheduler;
use symphony_cc::store::Store;
use symphony_cc::tracker::TrackerError;
use symphony_cc::tracker::fake::FakeTracker;
use symphony_cc::transcript::Transcripts;
use symphony_cc::worker::Session;
use symphony_cc::worker::fake::{FakeWorker, Script};
use symphony_cc::workspace::{DirWorkspace, GitWorktreeWorkspace, Workspace};

struct Harness {
    sched: Scheduler,
    clock: Arc<FakeClock>,
    tracker: Arc<FakeTracker>,
    worker: Arc<FakeWorker>,
    root: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn issue(n: u32, state: &str, prio: Option<i32>) -> Issue {
    Issue {
        id: format!("iss-{n}"),
        identifier: format!("MT-{n}"),
        title: format!("issue {n}"),
        body: None,
        state: state.to_string(),
        priority: prio,
        url: None,
        labels: vec!["agent".into()],
        dispatchable: true,
        created_at: Some(1_000 + n as i64),
        native_ref: None,
        blocked_by: vec![],
    }
}

fn harness(issues: Vec<Issue>, tune: impl FnOnce(&mut Config)) -> Harness {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!("symphony-sched-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let workspace = Arc::new(DirWorkspace::new(&root).unwrap());
    harness_over(issues, root, Store::open_in_memory().unwrap(), workspace, tune)
}

/// A harness whose durable state the caller names, so a second one can be built over the same
/// database and workspace root — which is all a restart is.
fn harness_over(
    issues: Vec<Issue>,
    root: PathBuf,
    store: Store,
    workspace: Arc<dyn Workspace>,
    tune: impl FnOnce(&mut Config),
) -> Harness {
    harness_full(issues, root, store, workspace, Arc::new(NoopProjector), tune)
}

/// The fully explicit builder. Every other harness reaches the scheduler through here, so a
/// test that needs to swap one seam names it and inherits the rest.
fn harness_full(
    issues: Vec<Issue>,
    root: PathBuf,
    store: Store,
    workspace: Arc<dyn Workspace>,
    projector: Arc<dyn Projector>,
    tune: impl FnOnce(&mut Config),
) -> Harness {
    let mut cfg = Config {
        tracker: TrackerConfig {
            kind: "fake".into(),
            active_states: vec!["in progress".into()],
            terminal_states: vec!["done".into()],
            required_labels: vec![],
            owner: String::new(),
            repo: String::new(),
        },
        polling: PollingConfig { interval_ms: 30_000 },
        workspace: WorkspaceConfig { root: Some(root.clone()), repo: None },
        agent: AgentConfig::default(),
        worker: Default::default(),
        broker: Default::default(),
        api: Default::default(),
        transcripts: Default::default(),
    };
    tune(&mut cfg);
    cfg.preflight().expect("test config must be valid");

    let clock = Arc::new(FakeClock::new());
    let tracker = Arc::new(FakeTracker::new(issues));
    let worker = Arc::new(FakeWorker::new(clock.clone()));

    let sched = Scheduler::new(
        cfg,
        clock.clone(),
        store,
        tracker.clone(),
        worker.clone(),
        workspace,
        projector,
    );

    Harness { sched, clock, tracker, worker, root }
}

/// A broker with nothing real behind it, for asserting on what the scheduler hands a run.
fn test_broker(clock: Arc<FakeClock>) -> (Arc<Broker>, Arc<FakeWrites>) {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("symphony-sched-mcp-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let writes = Arc::new(FakeWrites::new());
    let w: Arc<dyn TrackerWrites> = writes.clone();
    let b = Broker::new(
        w,
        clock,
        BrokerLimits::default(),
        vec!["in progress".into(), "done".into()],
        "127.0.0.1:1".parse().unwrap(),
        dir,
    )
    .unwrap();
    (Arc::new(b), writes)
}

// ---- tool broker ------------------------------------------------------------

#[test]
fn a_dispatched_run_is_handed_a_broker_endpoint_scoped_to_its_own_issue() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    let (broker, writes) = test_broker(h.clock.clone());
    h.sched.set_broker(Some(broker.clone()));

    h.sched.tick().unwrap();

    let endpoints = h.worker.endpoints_for("iss-1");
    assert_eq!(endpoints.len(), 1);
    let ep = endpoints[0].as_ref().expect("a broker was attached, so the run gets tools");
    assert_eq!(ep.server, "symphony");
    assert!(ep.config_path.exists(), "the worker needs a file to pass to --mcp-config");
    assert_eq!(ep.qualified("comment"), "mcp__symphony__comment");

    // The endpoint is only useful if the token inside it reaches this issue and no other. Read
    // it back the way the agent would, rather than trusting the scheduler's bookkeeping.
    let cfg: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ep.config_path).unwrap()).unwrap();
    let url = cfg["mcpServers"]["symphony"]["url"].as_str().unwrap();
    let token = url.rsplit('/').next().unwrap();

    broker.call(token, "comment", &serde_json::json!({ "body": "from the agent" })).unwrap();
    assert_eq!(writes.count(), 1);
    assert_eq!(writes.writes()[0].issue_id(), "iss-1");
}

#[test]
fn dispatch_without_a_broker_still_runs_the_agent_just_without_tools() {
    // The degrade the whole design turns on: a broker that could not start costs the agent a
    // capability, never a dispatch.
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});

    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 1, "the run happens regardless");
    assert_eq!(h.worker.endpoints_for("iss-1"), vec![None]);
}

#[test]
fn a_run_that_ends_takes_its_broker_authority_with_it() {
    // A token that outlived its run would let an orphaned agent keep writing to a ticket the
    // orchestrator has already released.
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    let (broker, writes) = test_broker(h.clock.clone());
    h.sched.set_broker(Some(broker.clone()));

    h.sched.tick().unwrap();
    let ep = h.worker.endpoints_for("iss-1")[0].clone().unwrap();
    let cfg: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ep.config_path).unwrap()).unwrap();
    let url = cfg["mcpServers"]["symphony"]["url"].as_str().unwrap().to_string();
    let token = url.rsplit('/').next().unwrap().to_string();
    assert_eq!(broker.open_sessions(), 1);

    // Let the scripted run finish and be harvested.
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();
    assert_eq!(
        h.sched.running_count(),
        0,
        "the run must actually be over for this to mean anything"
    );

    assert_eq!(broker.open_sessions(), 0, "the session died with the run");
    assert!(!ep.config_path.exists(), "and so did the file carrying its token");
    assert!(broker.call(&token, "comment", &serde_json::json!({ "body": "late" })).is_err());
    assert_eq!(writes.count(), 0);
}

// ---- dispatch ---------------------------------------------------------------

#[test]
fn dispatch_respects_the_global_concurrency_limit() {
    let issues = (1..=5).map(|n| issue(n, "In Progress", Some(1))).collect();
    let mut h = harness(issues, |c| c.agent.max_concurrent = 2);

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 2, "must not exceed max_concurrent");

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 2, "and must not creep upward on later ticks");
}

#[test]
fn a_saturated_state_cap_skips_that_state_without_stalling_the_queue() {
    let mut issues: Vec<Issue> = (1..=3).map(|n| issue(n, "In Progress", Some(1))).collect();
    issues.push(issue(9, "In Review", Some(1)));

    let mut h = harness(issues, |c| {
        c.tracker.active_states = vec!["in progress".into(), "in review".into()];
        c.agent.max_concurrent = 10;
        c.agent.max_concurrent_by_state.insert("in progress".into(), 1);
    });

    h.sched.tick().unwrap();
    // One "In Progress" plus the unconstrained "In Review": the per-state cap must not act as
    // a barrier for the rest of the queue.
    assert_eq!(h.sched.running_count(), 2);
}

#[test]
fn undispatchable_and_unlabelled_issues_are_never_picked_up() {
    let mut a = issue(1, "In Progress", Some(1));
    a.dispatchable = false;
    let mut b = issue(2, "In Progress", Some(1));
    b.labels = vec!["other".into()];
    let c = issue(3, "In Progress", Some(1));

    let mut h = harness(vec![a, b, c], |cfg| {
        cfg.tracker.required_labels = vec!["agent".into()];
    });

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "only the routable, correctly-labelled issue runs");
}

#[test]
fn dispatch_order_is_priority_then_age() {
    let issues = vec![
        issue(1, "In Progress", None), // no priority: last
        issue(2, "In Progress", Some(3)),
        issue(3, "In Progress", Some(1)), // highest
    ];
    let mut h = harness(issues, |c| c.agent.max_concurrent = 1);

    h.sched.tick().unwrap();
    let snap = h.sched.snapshot().unwrap();
    let running: Vec<_> = snap.rows.iter().filter(|r| r.phase == Phase::Running).collect();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].identifier, "MT-3");
}

// ---- outcomes ---------------------------------------------------------------

#[test]
fn a_done_verdict_releases_the_claim_and_keeps_the_workspace() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(1_000));

    h.sched.tick().unwrap();
    let ws = h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap();

    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 0);
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert!(!st.is_quarantined());
    assert_eq!(st.attempt, 0, "a clean finish must not leave failure state behind");
    assert!(
        PathBuf::from(&ws).exists(),
        "workspaces are preserved across runs; that warmth is the point"
    );
}

/// Releasing the claim is not enough on its own: the ticket usually stays in an active state
/// after a run finishes, so without parking the next tick picks it straight back up — the
/// continuation runaway arriving by another route.
#[test]
fn a_finished_issue_is_not_re_dispatched_while_its_state_is_unchanged() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(1_000));

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0);

    for _ in 0..10 {
        h.clock.advance_ms(60_000);
        h.sched.tick().unwrap();
        assert_eq!(h.sched.running_count(), 0, "parked issues must stay parked");
    }
    assert_eq!(
        h.sched.store().get("iss-1").unwrap().unwrap().parked_state.as_deref(),
        Some("in progress")
    );
}

#[test]
fn moving_the_ticket_makes_a_parked_issue_live_again() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.tracker.active_states = vec!["in progress".into(), "in review".into()];
    });
    h.worker.set_default(Script::succeeds_in(1_000));

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "parked");

    // A human moves it on: there is something new to act on.
    h.tracker.set_state("iss-1", "In Review");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "a state change lifts the park");
    assert!(h.sched.store().get("iss-1").unwrap().unwrap().parked_state.is_none());
}

/// Bring a fresh harness to the point where `iss-1` has finished `Done` and is parked, and
/// return its workspace path. The first tick dispatches; the second harvests and parks.
fn park_one(h: &mut Harness) -> PathBuf {
    h.worker.set_default(Script::succeeds_in(1_000));
    h.sched.tick().unwrap();
    let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "parked");
    assert!(ws.exists(), "a parked issue keeps its warm workspace");
    ws
}

/// The leak this closes: a `Done` run is released and parked, which takes it out of `running`,
/// and once the ticket closes the active-state poll never returns it again. Every cleanup path
/// before this one hung off `running` or a retry row, so the worktree stayed on disk for good.
#[test]
fn a_parked_issue_that_is_later_closed_has_its_workspace_reclaimed_without_a_restart() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.parked_sweep_interval_ms = 300_000;
    });
    let ws = park_one(&mut h);

    // While the ticket stays where the run left it, sweeps find nothing to reclaim.
    h.clock.advance_ms(300_000);
    h.sched.tick().unwrap();
    assert!(ws.exists(), "a parked issue in a non-terminal state keeps its workspace");

    // A human closes it. The next sweep is what notices — no restart, no re-dispatch.
    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(300_000);
    h.sched.tick().unwrap();

    assert!(!ws.exists(), "the closed issue's workspace must be reclaimed");
    assert_eq!(h.sched.running_count(), 0, "cleanup must not dispatch anything");
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert!(
        st.parked_state.is_none(),
        "a reclaimed issue leaves the parked set, or every later sweep would pay for it"
    );
    assert_eq!(st.phase, Phase::Released);
}

/// The sweep is the one path that reaches `Workspace::remove` with no live run behind it, so
/// it is the one a reviewer forgets when `remove`'s contract changes. It did: the branch a run
/// leaves behind is published from the store, and cleanup deletes that ref whenever git's
/// merged check says it carries nothing new — so a sweep that reclaims a worktree without
/// clearing the stored name leaves `status` pointing at a branch that is gone.
#[test]
fn a_sweep_that_deletes_a_branch_clears_the_name_the_snapshot_publishes() {
    let dir = tmp_dir("sweep-clears-branch");
    let root = dir.join("workspaces");
    let repo = git_repo(&dir.join("repo"));

    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open_in_memory().unwrap(),
        Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
        |c| c.agent.parked_sweep_interval_ms = 300_000,
    );

    // A run that finishes without committing: cleanup will delete its branch, because the
    // merged check finds nothing on it that HEAD does not already have.
    h.worker.set_default(Script::succeeds_in(1_000));
    h.sched.tick().unwrap();
    // `Row.workspace` describes a live run, so it has to be read while the run is still in
    // flight. `Row.branch` comes from the store and outlives it — which is the point.
    let ws = PathBuf::from(
        h.sched.snapshot().unwrap().rows[0]
            .workspace
            .clone()
            .expect("a running row names its worktree"),
    );
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "the run is parked, not running");

    let parked = h.sched.snapshot().unwrap();
    let row = parked.rows.iter().find(|r| r.issue_id == "iss-1").unwrap();
    assert!(row.branch.is_some(), "a parked run still publishes the branch it checked out");
    assert!(ws.exists(), "a parked issue keeps its warm workspace");

    // The ticket closes later, so the sweep is what reclaims it — no restart, no re-dispatch.
    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(300_000);
    h.sched.tick().unwrap();

    assert!(!ws.exists(), "the sweep must reclaim the closed issue's workspace");
    let after = h.sched.snapshot().unwrap();
    let row = after.rows.iter().find(|r| r.issue_id == "iss-1").unwrap();
    assert_eq!(
        row.branch, None,
        "the sweep deleted the ref, so the published name must go with it rather than \
         sending an operator to a branch that no longer exists"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The sweep is a poll over ids the scheduler is otherwise not watching, so its cost has to be
/// set by the interval and the number of parked issues — not by how many ticks an issue spends
/// parked. An implementation that swept every tick would pass the test above and fail this one.
#[test]
fn sweeping_parked_issues_costs_tracker_traffic_bounded_by_the_interval_not_by_ticks() {
    let interval = 300_000u64;
    let poll = 30_000u64;
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.parked_sweep_interval_ms = interval;
    });
    park_one(&mut h);
    let (_, by_ids_before) = h.tracker.call_counts();

    let ticks = 20u64;
    for _ in 0..ticks {
        h.clock.advance_ms(poll);
        h.sched.tick().unwrap();
    }

    let (_, by_ids_after) = h.tracker.call_counts();
    let sweeps = (by_ids_after - by_ids_before) as u64;
    let elapsed = ticks * poll;
    assert!(sweeps >= 1, "the sweep has to run at all for the bound to mean anything");
    assert!(
        sweeps <= elapsed / interval + 1,
        "{sweeps} by_ids calls over {elapsed}ms with a {interval}ms interval: the sweep is \
         running more often than its cadence allows"
    );
    assert!(sweeps < ticks, "and it certainly must not run once per tick");
}

/// Reclaiming a parked worktree must keep the branch-survival contract that every other cleanup
/// path keeps: the directory is scratch, the commits are the deliverable. Closing the ticket is
/// not a decision to throw the work away.
#[test]
fn a_parked_issues_committed_work_survives_the_sweep_that_reclaims_its_worktree() {
    let dir = tmp_dir("parked-sweep-git");
    let root = dir.join("workspaces");
    let repo = git_repo(&dir.join("repo"));
    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open_in_memory().unwrap(),
        Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
        |c| c.agent.parked_sweep_interval_ms = 300_000,
    );
    let ws = park_one(&mut h);
    commit_in(&ws, "work.txt", "what the agent delivered");
    std::fs::write(ws.join("scratch.txt"), b"never committed").unwrap();

    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(300_000);
    h.sched.tick().unwrap();
    assert!(!ws.exists(), "the worktree itself must be reclaimed");

    // Reopening is how the branch is proven to have survived: the next dispatch attaches to
    // it, so the committed file comes back while the uncommitted one stays gone.
    h.tracker.set_state("iss-1", "In Progress");
    h.worker.set_default(Script::succeeds_in(600_000));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "a reopened issue is dispatchable again");
    assert!(ws.join("work.txt").exists(), "the commits must outlive the worktree they were in");
    assert!(
        !ws.join("scratch.txt").exists(),
        "which is only meaningful if the directory was rebuilt"
    );

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_blocked_verdict_releases_without_scheduling_a_retry() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Blocked { why: "needs a human".into() }),
    );

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 0);
    assert!(h.sched.store().all_retries().unwrap().is_empty(), "blocked must not retry");
}

/// The spec's central defect: a clean exit means "maybe continue", re-dispatched on a flat 1s
/// timer forever. Here the delay escalates while the tracker state does not move.
#[test]
fn continuation_backs_off_instead_of_respawning_every_second() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );

    // First continuation: 5s.
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let due = h.sched.store().all_retries().unwrap()[0].due_at;
    assert_eq!(due - h.clock.wall().0, 5_000);

    // Second, with the state still unmoved: 30s.
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let due = h.sched.store().all_retries().unwrap()[0].due_at;
    assert_eq!(due - h.clock.wall().0, 30_000);
}

/// A continuation is only worth having if it continues something. Respawning `claude -p` cold
/// makes the agent re-read the issue and re-explore the tree on every turn the budget buys, so
/// the budget and the restart spend the same turns twice.
#[test]
fn a_continuation_resumes_the_conversation_the_first_attempt_started() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // harvest the Continue, queue the retry
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap(); // the continuation itself

    let seen = h.worker.sessions_for("iss-1");
    assert_eq!(seen.len(), 2, "expected a first attempt and a continuation, got {seen:?}");
    assert!(matches!(seen[0], Session::New(_)), "the first attempt names a fresh conversation");
    assert_eq!(
        seen[1],
        Session::Resume(seen[0].id().to_string()),
        "the continuation must resume the conversation the first attempt started"
    );
}

/// The degradation path. A session the CLI no longer holds is answered with "No conversation
/// found with session ID" and no turns at all, so resuming the same name again fails identically
/// every time — straight into quarantine, for an issue with nothing wrong with it.
#[test]
fn a_run_that_took_no_turns_is_not_retried_into_the_same_conversation() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script {
        turns: 0,
        outcome: Outcome::Failed {
            class: ErrorClass::AgentCrash,
            msg: "no conversation found with session id".into(),
        },
        ..Script::succeeds_in(1_000)
    });

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // harvest the failure, queue the retry
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap(); // the retry

    let seen = h.worker.sessions_for("iss-1");
    assert_eq!(seen.len(), 2, "expected a first attempt and a retry, got {seen:?}");
    assert!(
        matches!(seen[1], Session::New(_)),
        "a conversation that produced nothing must not be resumed into, got {seen:?}"
    );
    assert_ne!(seen[0].id(), seen[1].id(), "the retry needs a name of its own");
}

/// The other half of the brake: even a well-behaved `Continue` cannot run forever.
#[test]
fn the_per_issue_turn_budget_stops_an_endless_continuation() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.max_turns_per_issue = 5; // the fake burns 3 turns per run
    });
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "forever".into() }),
    );

    for _ in 0..12 {
        h.sched.tick().unwrap();
        h.clock.advance_ms(31_000);
    }

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert!(st.is_quarantined(), "must stop once the cross-session turn budget is spent");
    assert!(
        st.cumulative_turns >= 5,
        "budget should have been reached, saw {}",
        st.cumulative_turns
    );
}

// ---- failures ---------------------------------------------------------------

#[test]
fn a_permanent_failure_quarantines_immediately_rather_than_retrying_forever() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(1_000).with_outcome(Outcome::Failed {
        class: ErrorClass::TemplateRender,
        msg: "unknown variable".into(),
    }));

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert!(st.is_quarantined());
    assert!(h.sched.store().all_retries().unwrap().is_empty());

    // And it stays out of the rotation on later ticks.
    h.clock.advance_ms(600_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0);
}

#[test]
fn a_quarantined_issue_returns_to_service_once_an_operator_clears_it() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Failed { class: ErrorClass::AuthFailed, msg: "401".into() }),
    );

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert!(h.sched.store().get("iss-1").unwrap().unwrap().is_quarantined());

    h.worker.set_default(Script::succeeds_in(1_000));
    h.sched.unquarantine("iss-1").unwrap();
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "cleared issues become dispatchable again");
}

#[test]
fn repeated_transient_failures_escalate_then_quarantine() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.quarantine_after_identical = 3;
    });
    h.worker.set_default(
        Script::succeeds_in(500)
            .with_outcome(Outcome::Failed { class: ErrorClass::AgentCrash, msg: "boom".into() }),
    );

    let mut delays = Vec::new();
    for _ in 0..3 {
        h.sched.tick().unwrap();
        h.clock.advance_ms(500);
        h.sched.tick().unwrap();
        if let Some(r) = h.sched.store().all_retries().unwrap().first() {
            delays.push(r.due_at - h.clock.wall().0);
        }
        h.clock.advance_ms(400_000); // past any scheduled backoff
    }

    assert_eq!(delays, vec![10_000, 20_000], "backoff doubles between attempts");
    assert!(
        h.sched.store().get("iss-1").unwrap().unwrap().is_quarantined(),
        "an identical failure three times running is not transient"
    );
}

#[test]
fn a_stalled_run_is_terminated_and_retried() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.stall_timeout_ms = 10_000;
    });
    h.worker.set_default(Script::stalls_after(1_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);

    h.clock.advance_ms(2_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "still producing output; not yet stalled");

    h.clock.advance_ms(30_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "silence past the timeout must terminate the run");
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.last_error_class, Some(ErrorClass::Stall));
}

// ---- reconciliation ---------------------------------------------------------

#[test]
fn a_ticket_moving_to_terminal_stops_the_run_and_cleans_up() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    let ws = h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap();
    assert!(PathBuf::from(&ws).exists());

    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 0);
    assert!(!PathBuf::from(&ws).exists(), "terminal issues have their workspace removed");
}

#[test]
fn a_ticket_leaving_the_active_set_stops_the_run_but_keeps_the_workspace() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    let ws = h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap();

    h.tracker.set_state("iss-1", "Backlog"); // neither active nor terminal
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 0);
    assert!(PathBuf::from(&ws).exists(), "non-terminal exits must not discard warm state");
}

/// A `RunHandle` outlives the `Scheduler` unless something kills it explicitly — found
/// empirically the first time a real worker process was left running after `cargo run`
/// returned with a run still in flight, because nothing had ever called `terminate` on it.
#[test]
fn shutdown_stops_every_in_flight_run_without_discarding_its_workspace() {
    let mut h =
        harness(vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(1))], |c| {
            c.agent.max_concurrent = 2;
        });
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 2);
    let workspaces: Vec<PathBuf> = h
        .sched
        .snapshot()
        .unwrap()
        .rows
        .iter()
        .map(|r| PathBuf::from(r.workspace.clone().unwrap()))
        .collect();

    h.sched.shutdown().unwrap();

    assert_eq!(h.sched.running_count(), 0);
    for ws in &workspaces {
        assert!(ws.exists(), "shutdown must not discard warm state, only stop the run");
    }

    // The claim must be released too, or the issue would never be dispatchable again.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 2, "a released issue is picked back up on the next tick");
}

/// The spec kills on the first refresh miss, so a single eventual-consistency blip destroys
/// in-flight work.
#[test]
fn one_invisible_refresh_is_survivable_but_two_are_not() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.refresh_miss_grace = 2;
    });
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);

    h.tracker.hide("iss-1");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "one miss is within grace");

    h.tracker.unhide("iss-1");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "reappearing resets the grace count");

    h.tracker.hide("iss-1");
    for _ in 0..2 {
        h.clock.advance_ms(1_000);
        h.sched.tick().unwrap();
    }
    assert_eq!(h.sched.running_count(), 0, "two consecutive misses is gone");
}

#[test]
fn a_tracker_outage_keeps_existing_workers_alive() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);

    h.tracker.fail_by_ids(Some(TrackerError::RateLimited));
    h.tracker.fail_by_states(Some(TrackerError::RateLimited));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 1, "an outage must not cancel real work");
    assert!(h.sched.snapshot().unwrap().last_error.is_some(), "but it must be visible");
}

// ---- config gating ----------------------------------------------------------

#[test]
fn a_broken_config_stops_dispatch_but_still_reconciles() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(1_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);

    // Break it the way a bad hot-reload would.
    h.sched.cfg.tracker.active_states.clear();

    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    // The finished run was still harvested despite dispatch being gated off.
    assert_eq!(h.sched.running_count(), 0);
    let snap = h.sched.snapshot().unwrap();
    assert!(snap.last_error.unwrap().contains("preflight"));
}

#[test]
fn nothing_is_dispatched_twice_across_repeated_ticks() {
    let issues = (1..=3).map(|n| issue(n, "In Progress", Some(1))).collect();
    let mut h = harness(issues, |c| c.agent.max_concurrent = 5);
    h.worker.set_default(Script::succeeds_in(600_000));

    for _ in 0..5 {
        h.sched.tick().unwrap();
        h.clock.advance_ms(100);
    }

    assert_eq!(h.sched.running_count(), 3, "claims must make re-dispatch impossible");
    let snap = h.sched.snapshot().unwrap();
    assert_eq!(snap.rows.iter().filter(|r| r.phase == Phase::Running).count(), 3);
}

// ---- startup recovery -------------------------------------------------------

fn tmp_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("symphony-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn git(at: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").arg("-C").arg(at).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
}

/// A throwaway repo with one commit, so `git worktree add` has a HEAD to branch from. Identity
/// is set repo-local rather than relying on the machine having a global one.
fn git_repo(at: &Path) -> PathBuf {
    std::fs::create_dir_all(at).unwrap();
    git(at, &["init", "-q", "-b", "main"]);
    git(at, &["config", "user.email", "test@example.com"]);
    git(at, &["config", "user.name", "test"]);
    git(at, &["commit", "-q", "--allow-empty", "-m", "init"]);
    at.canonicalize().unwrap()
}

/// Commit inside a worktree, standing in for work a dispatched agent finished before the kill.
fn commit_in(worktree: &Path, file: &str, msg: &str) {
    std::fs::write(worktree.join(file), msg.as_bytes()).unwrap();
    git(worktree, &["add", file]);
    git(worktree, &["commit", "-q", "-m", msg]);
}

/// A restarted process gets a `FakeClock` that starts where the dead one's did, which no real
/// restart does. Run ids are `issue_id`-plus-wall-millisecond, so without this the re-dispatch
/// collides with the row the interrupted run left behind.
fn restart_took_time(h: &Harness) {
    h.clock.advance_ms(1_000);
}

/// A `SIGKILL` runs no destructor. `shutdown()`, `Drop for Scheduler` and the interrupt arm in
/// `main` all release in-flight claims; a hard kill, an OOM kill and a host reboot reach none of
/// them. `mem::forget` is the faithful simulation: the store keeps the claim, the disk keeps the
/// worktree, and the in-memory `running` map dies with the process that owned it.
#[test]
fn a_claim_stranded_by_a_hard_kill_is_recovered_at_the_next_startup() {
    let dir = tmp_dir("hard-kill");
    let db = dir.join("symphony.db");
    let root = dir.join("workspaces");

    let ws = {
        let mut h = harness_over(
            vec![issue(1, "In Progress", Some(1))],
            root.clone(),
            Store::open(&db).unwrap(),
            Arc::new(DirWorkspace::new(&root).unwrap()),
            |_| {},
        );
        h.worker.set_default(Script::succeeds_in(600_000));
        h.sched.tick().unwrap();
        assert_eq!(h.sched.running_count(), 1);

        let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
        // Stands in for whatever the kill catches an agent half-way through writing.
        std::fs::write(ws.join("half-written.txt"), b"caught mid-run").unwrap();

        std::mem::forget(h); // the kill: no shutdown, no Drop, nothing released
        ws
    };

    // What the dead process left behind: a claim no live run can ever match again.
    let phase = Store::open(&db).unwrap().get("iss-1").unwrap().unwrap().phase;
    assert_eq!(phase, Phase::Running, "the claim outlives the process that took it");

    // The restart. Recovery runs inside the first tick, so this is the ordinary startup path
    // rather than a call some second entry point has to remember to make.
    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open(&db).unwrap(),
        Arc::new(DirWorkspace::new(&root).unwrap()),
        |_| {},
    );
    h.worker.set_default(Script::succeeds_in(600_000));
    restart_took_time(&h);
    h.sched.tick().unwrap();

    // Being dispatchable at all is the point: without recovery `claim()` refuses forever.
    assert_eq!(h.sched.running_count(), 1, "a stranded issue must be dispatched again");
    assert!(
        !ws.join("half-written.txt").exists(),
        "the orphaned worktree must be reconciled, not handed on as the kill left it"
    );

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The cleanup that frees a stranded issue must not be what discards what the killed run had
/// already committed. The directory is scratch space; the branch is the deliverable.
#[test]
fn a_hard_killed_runs_commits_survive_the_recovery_that_frees_its_issue() {
    let dir = tmp_dir("hard-kill-git");
    let db = dir.join("symphony.db");
    let root = dir.join("workspaces");
    let repo = git_repo(&dir.join("repo"));

    let ws = {
        let mut h = harness_over(
            vec![issue(1, "In Progress", Some(1))],
            root.clone(),
            Store::open(&db).unwrap(),
            Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
            |_| {},
        );
        h.worker.set_default(Script::succeeds_in(600_000));
        h.sched.tick().unwrap();

        let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
        commit_in(&ws, "work.txt", "what the agent finished before the kill");
        std::fs::write(ws.join("scratch.txt"), b"never committed").unwrap();

        std::mem::forget(h);
        ws
    };

    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open(&db).unwrap(),
        Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
        |_| {},
    );
    h.worker.set_default(Script::succeeds_in(600_000));
    restart_took_time(&h);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 1, "the stranded issue must be dispatchable again");
    // The uncommitted file proves the worktree really was removed and rebuilt, so the committed
    // one coming back proves the branch it was removed with outlived the reconciliation.
    assert!(!ws.join("scratch.txt").exists(), "the orphaned worktree itself must be reconciled");
    assert!(ws.join("work.txt").exists(), "but the commits it held must survive that");

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The filter that makes recovery safe to run at all: a claim is stale only when *this* process
/// has no run for it. Drop the check and a recovery pass reconciles away the live work it was
/// added to rescue.
#[test]
fn recovery_leaves_the_claims_of_runs_this_process_is_still_executing_alone() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());

    h.sched.recover().unwrap();

    assert_eq!(h.sched.running_count(), 1, "a live run must not be reconciled away");
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().phase, Phase::Running);
    assert!(ws.exists(), "nor its workspace removed out from under it");
}

// ---- published branch ---------------------------------------------------------

/// Review finding on PR #31 (`src/sched/mod.rs:895`): the published branch used to be
/// recomputed on every snapshot from `st.identifier` and "has this issue ever produced a run",
/// neither of which proves the recomputed name is the branch `prepare` actually checked out.
/// Two things break it independently: `Store::ensure` can rename `identifier` after dispatch,
/// changing what a recompute produces, and `Workspace::remove` deletes a branch that turns out
/// to carry no commits while the run history that gated the old derivation never clears. This
/// drives both: one issue's branch survives cleanup because it holds a commit, the other's does
/// not, and the surviving one's identifier is renamed after dispatch. The recorded value must
/// track what `prepare` returned and what cleanup actually did, not what a recompute would say.
#[test]
fn the_published_branch_is_the_one_prepare_recorded_not_one_recomputed_from_the_current_identifier()
{
    let dir = tmp_dir("branch-recorded-not-derived");
    let root = dir.join("workspaces");
    let repo = git_repo(&dir.join("repo"));

    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(1))],
        root.clone(),
        Store::open_in_memory().unwrap(),
        Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
        |c| c.agent.max_concurrent = 2,
    );
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    let snap = h.sched.snapshot().unwrap();
    let row_1 = snap.rows.iter().find(|r| r.issue_id == "iss-1").unwrap();
    let row_2 = snap.rows.iter().find(|r| r.issue_id == "iss-2").unwrap();
    assert!(row_1.branch.is_some(), "a dispatched run reports the branch it checked out");
    let branch_2 = row_2.branch.clone().expect("same for the second issue");
    let ws_2 = PathBuf::from(row_2.workspace.clone().unwrap());

    // iss-2's run leaves a commit, so cleanup keeps its branch; iss-1's leaves none, so cleanup
    // deletes it (git's own merged check, exercised by `GitWorktreeWorkspace::remove`).
    commit_in(&ws_2, "work.txt", "the agent's output");

    // A tracker-side rename after dispatch — the failure mode a recomputed name cannot survive.
    // `worktree_key` is fixed at the first `ensure` and ignored on conflict, so this only
    // changes what `identifier` displays as, never which worktree or branch this issue owns.
    h.sched.store().ensure(h.clock.as_ref(), "iss-2", "MT-2-renamed", "irrelevant").unwrap();

    h.tracker.set_state("iss-1", "Done");
    h.tracker.set_state("iss-2", "Done");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let snap = h.sched.snapshot().unwrap();
    let row_1 = snap.rows.iter().find(|r| r.issue_id == "iss-1").unwrap();
    let row_2 = snap.rows.iter().find(|r| r.issue_id == "iss-2").unwrap();

    assert_eq!(row_2.identifier, "MT-2-renamed", "the rename really did take hold in the store");
    assert_eq!(
        row_1.branch, None,
        "cleanup deleted the ref this run actually used; the stored value must follow, not \
         keep reporting a name that no longer exists just because the issue has run history"
    );
    assert_eq!(
        row_2.branch,
        Some(branch_2),
        "cleanup kept this branch because it holds commits, so the recorded name must survive \
         unchanged — not get recomputed from the identifier that was renamed underneath it"
    );

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- projection -------------------------------------------------------------

/// The projector every real deployment is one disk error away from.
struct FailingProjector;

impl Projector for FailingProjector {
    fn project(&self, _issues: &[symphony_cc::project::ProjectedIssue]) -> anyhow::Result<()> {
        anyhow::bail!("disk full")
    }
}

/// Everything the scheduler decides, as one comparable value per tick: the store is its
/// judgment, the retry table its timing, the snapshot what it tells the world. The snapshot
/// has no `PartialEq`, so its `Debug` form stands in — every field in it is deterministic on a
/// fake clock, including the timestamps. The one thing that legitimately differs between two
/// otherwise identical lifetimes is where on disk each one lived, so the workspace root is
/// masked out rather than letting it hide a real divergence behind an expected one.
fn decision_trace(h: &Harness, root: &Path) -> String {
    let store = h.sched.store();
    format!(
        "running={}\nstates={:?}\nretries={:?}\nsnapshot={:?}",
        h.sched.running_count(),
        store.all().unwrap(),
        store.all_retries().unwrap(),
        h.sched.snapshot().unwrap(),
    )
    .replace(&*root.to_string_lossy(), "<root>")
}

/// One scripted lifetime exercising every path `publish` runs after: a clean finish, a
/// retryable failure with its retry coming due, an immediate quarantine, a continuation, and
/// a ticket reaching a terminal state with the cleanup that triggers. Returns the trace after
/// every tick, so a divergence names the tick it happened on.
fn run_scripted_lifetime(projector: Arc<dyn Projector>, root: PathBuf) -> Vec<String> {
    let workspace = Arc::new(DirWorkspace::new(&root).unwrap());
    // `DirWorkspace` canonicalises, so the paths in the snapshot are the resolved form, not
    // the one this function was handed — on macOS that is `/private/var/...` for `/var/...`.
    let canonical_root = workspace.root().to_path_buf();
    let mut h = harness_full(
        vec![
            issue(1, "In Progress", Some(1)),
            issue(2, "In Progress", Some(2)),
            issue(3, "In Progress", Some(3)),
            issue(4, "In Progress", Some(4)),
        ],
        root,
        Store::open_in_memory().unwrap(),
        workspace,
        projector,
        |c| c.agent.max_concurrent = 4,
    );
    h.worker.script("iss-1", Script::succeeds_in(1_000));
    h.worker.script(
        "iss-2",
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Failed { class: ErrorClass::Stall, msg: "hung".into() }),
    );
    h.worker.script(
        "iss-3",
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Failed { class: ErrorClass::AuthFailed, msg: "401".into() }),
    );
    h.worker.script(
        "iss-4",
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more".into() }),
    );

    let mut trace = Vec::new();
    h.sched.tick().unwrap();
    trace.push(decision_trace(&h, &canonical_root));

    // Every run finishes; verdicts land: done, retry scheduled, quarantined, continuation.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    trace.push(decision_trace(&h, &canonical_root));

    // A human closes the finished one; cleanup fires.
    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    trace.push(decision_trace(&h, &canonical_root));

    // Far enough for both the continuation and the first backoff to come due.
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();
    trace.push(decision_trace(&h, &canonical_root));

    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    trace.push(decision_trace(&h, &canonical_root));

    trace
}

/// The projection is a view. Whether it writes real task files, fails on every call, or is
/// switched off entirely, the scheduler must reach the same decisions on the same ticks —
/// otherwise the dashboard has become load-bearing, which is the failure this invariant
/// exists to make impossible. Proved by trace, not by inspection: the same scripted lifetime
/// runs three times and the traces are required to be identical.
#[test]
fn the_scheduler_makes_the_same_decisions_whether_the_projector_writes_fails_or_is_off() {
    let dir = tmp_dir("projector-bit-identical");

    let off = run_scripted_lifetime(Arc::new(NoopProjector), dir.join("ws-off"));

    let failing = run_scripted_lifetime(Arc::new(FailingProjector), dir.join("ws-failing"));
    assert_eq!(off, failing, "a projector that fails on every call must not change one decision");

    let tasks_root = dir.join("tasks");
    let real = TasksProjector::new(&tasks_root, "sess").unwrap();
    let written = run_scripted_lifetime(Arc::new(real), dir.join("ws-real"));
    assert_eq!(off, written, "a projector that writes real files must not change one decision");

    // Sanity: the real one did write — otherwise the third leg tested the same thing as the
    // first. Four issues, none ever dropped from the store, so four files.
    assert_eq!(
        std::fs::read_dir(tasks_root.join("sess")).unwrap().count(),
        4,
        "the real projector must have projected, or this test proves nothing about it"
    );

    // Each tick must also have differed from the last, or the trace is too coarse to catch a
    // divergence that happened to land on a quiet tick.
    for w in off.windows(2) {
        assert_ne!(w[0], w[1], "every scripted tick should move the scheduler somewhere new");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- transcripts ------------------------------------------------------------

/// A transcript root under the harness's own directory, so it is cleaned up with it.
fn transcript_root(h: &Harness) -> PathBuf {
    h.root.join(".transcripts")
}

#[test]
fn a_completed_run_leaves_a_readable_transcript_reachable_from_its_run_record() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    let root = transcript_root(&h);
    h.sched.set_transcripts(Some(Transcripts::new(&root, 1 << 20, 10).unwrap()));

    h.sched.tick().unwrap();
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().phase, Phase::Released);

    // The run record is the entry point — nobody should have to know the layout on disk.
    let runs = h.sched.store().runs_for("iss-1").unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome.as_deref(), Some("done"));
    let path = PathBuf::from(runs[0].transcript.clone().expect("the run recorded a transcript"));

    let text = std::fs::read_to_string(&path).expect("and the transcript is readable");
    assert!(text.contains("symphony_run_start"), "got: {text}");
    assert!(text.contains("iss-1"));
    assert!(text.lines().count() > 1, "a transcript with only a header records nothing");
}

#[test]
fn a_transcript_root_that_cannot_be_written_costs_the_record_and_not_the_dispatch() {
    let h0 = harness(vec![], |_| {});
    // A *file* where the root should be: `create_dir_all` fails, and so would every open under
    // it. The degrade has to be silent enough that dispatch never notices.
    std::fs::create_dir_all(&h0.root).unwrap();
    let blocked = h0.root.join("not-a-directory");
    std::fs::write(&blocked, b"").unwrap();
    assert!(Transcripts::new(&blocked, 1 << 20, 10).is_err());
    drop(h0);

    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.sched.set_transcripts(None);

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "a missing transcript must never cost a dispatch");

    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    let runs = h.sched.store().runs_for("iss-1").unwrap();
    assert_eq!(runs[0].outcome.as_deref(), Some("done"));
    assert_eq!(runs[0].transcript, None, "and the row says plainly that there is none");
}

/// Retention has to bound the directory without ever unlinking a file a live run is writing
/// to — and a stalled run is both live and, by definition, the oldest file there.
#[test]
fn retention_bounds_the_transcript_directory_but_spares_a_stalled_runs_own_file() {
    const KEEP: usize = 2;

    let mut h =
        harness(vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))], |c| {
            c.agent.stall_timeout_ms = 3_600_000
        });
    let root = transcript_root(&h);
    // Deliberately far below the number of runs this test produces, so pruning has to fire.
    h.sched.set_transcripts(Some(Transcripts::new(&root, 1 << 20, KEEP).unwrap()));

    // iss-1 goes quiet at once and never finishes — live throughout, and its file never gets a
    // second write, so it ages to the back of the directory while the process behind it runs.
    h.worker.script("iss-1", Script::stalls_after(0));
    // iss-2 keeps asking for another turn, so every retry opens a fresh run and a fresh file.
    h.worker.script(
        "iss-2",
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );

    h.sched.tick().unwrap();
    let stalled =
        PathBuf::from(h.sched.store().runs_for("iss-1").unwrap()[0].transcript.clone().unwrap());
    assert!(stalled.exists());

    // Past the escalating continuation backoff each time, so each pass really re-dispatches.
    for _ in 0..6 {
        h.clock.advance_ms(600_000);
        h.sched.tick().unwrap();
    }

    let created = h.sched.store().runs_for("iss-2").unwrap().len();
    assert!(created > KEEP + 1, "the test needs more runs than the bound: {created}");

    let kept = std::fs::read_dir(&root).unwrap().count();
    assert!(kept < created, "retention never fired: {kept} files for {created} runs");
    // The bound, plus the one file retention is not allowed to touch.
    assert!(kept <= KEEP + 1, "retention is not bounding the directory: {kept} files");
    assert!(
        stalled.exists(),
        "a live run's transcript must survive retention, however old the file looks"
    );
}
