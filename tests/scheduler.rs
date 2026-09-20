//! Scheduler behaviour, end to end, on a fake clock.
//!
//! Every test drives real `Scheduler::tick` against fake tracker/worker/workspace. Time only
//! moves when a test moves it, so there are no sleeps and nothing to flake. Each test names the
//! invariant it defends; several of them correspond directly to defects found in the spec.

use std::path::PathBuf;
use std::sync::Arc;

use symphony_cc::clock::{Clock, FakeClock};
use symphony_cc::config::{AgentConfig, Config, PollingConfig, TrackerConfig, WorkspaceConfig};
use symphony_cc::model::{ErrorClass, Issue, Outcome, Phase};
use symphony_cc::project::NoopProjector;
use symphony_cc::sched::Scheduler;
use symphony_cc::store::Store;
use symphony_cc::tracker::TrackerError;
use symphony_cc::tracker::fake::FakeTracker;
use symphony_cc::worker::fake::{FakeWorker, Script};
use symphony_cc::workspace::DirWorkspace;

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

    let mut cfg = Config {
        tracker: TrackerConfig {
            kind: "fake".into(),
            active_states: vec!["in progress".into()],
            terminal_states: vec!["done".into()],
            required_labels: vec![],
        },
        polling: PollingConfig { interval_ms: 30_000 },
        workspace: WorkspaceConfig { root: Some(root.clone()) },
        agent: AgentConfig::default(),
    };
    tune(&mut cfg);
    cfg.preflight().expect("test config must be valid");

    let clock = Arc::new(FakeClock::new());
    let tracker = Arc::new(FakeTracker::new(issues));
    let worker = Arc::new(FakeWorker::new(clock.clone()));
    let workspace = Arc::new(DirWorkspace::new(&root).unwrap());

    let sched = Scheduler::new(
        cfg,
        clock.clone(),
        Store::open_in_memory().unwrap(),
        tracker.clone(),
        worker.clone(),
        workspace,
        Arc::new(NoopProjector),
    );

    Harness { sched, clock, tracker, worker, root }
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
