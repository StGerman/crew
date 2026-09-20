//! The coordination layer: one tick, one authority, no model in the loop.
//!
//! Tick order is deliberate. Reconciliation runs *first and unconditionally*, so a broken
//! config stops new dispatch without also stranding the runs already in flight.
//!
//! Deviation from the plan: reconciliation lives here rather than in its own module, because
//! it mutates the same `running` map as dispatch and splitting it would mean threading the
//! whole scheduler through a free function for no readability gain.

pub mod retry;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::clock::{Clock, Mono, Wall};
use crate::config::Config;
use crate::model::{ErrorClass, Issue, Outcome, Phase, session_id, worktree_key};
use crate::project::{ProjectedIssue, Projector};
use crate::store::Store;
use crate::tracker::{Tracker, TrackerError};
use crate::worker::{Progress, RunHandle, Session, Worker};
use crate::workspace::Workspace;

/// Bounded wait for a worker to stop before the workspace may be touched.
const KILL_GRACE_MS: u64 = 10_000;

/// A tracker failure never stops the tick — every call site here already returns `Ok(())` and
/// tries again next poll — but a permanent one (a bad credential, most likely) will not
/// resolve itself, and retrying it silently at the poll interval forever looks identical to a
/// transient blip on `warn`-level logs alone. This is the one place that distinction is made,
/// so every call site sees it without repeating the branch.
fn log_tracker_failure(context: &str, e: &TrackerError) {
    if e.class().retryable() {
        tracing::warn!(error = %e, "{context}; will retry next poll");
    } else {
        tracing::error!(error = %e, "{context}; will not resolve on its own — check tracker credentials/config");
    }
}

struct Running {
    run_id: String,
    issue: Issue,
    handle: Arc<dyn RunHandle>,
    started: Mono,
    workspace: PathBuf,
    last_progress: Progress,
    last_progress_at: Mono,
    /// Tracker state when this run began, to tell real progress from spinning.
    state_at_start: String,
}

#[derive(Debug, Clone, Default)]
pub struct Row {
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub url: Option<String>,
    pub tracker_state: String,
    pub phase: Phase,
    pub attempt: u32,
    pub turns: u32,
    pub in_tok: u64,
    pub out_tok: u64,
    pub age_ms: u64,
    pub retry_in_ms: Option<i64>,
    pub quarantined: bool,
    pub last_error: Option<String>,
    pub last_event: Option<String>,
    pub workspace: Option<String>,
}

/// Immutable view published to the UI. The TUI renders this and never touches the store,
/// which is what keeps the dashboard from becoming load-bearing.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub generated_at: i64,
    pub rows: Vec<Row>,
    pub running: usize,
    pub limit: usize,
    pub retrying: usize,
    pub quarantined: usize,
    pub in_tok: u64,
    pub out_tok: u64,
    pub ticks: u64,
    pub last_tick_at: Option<i64>,
    pub last_error: Option<String>,
}

pub struct Scheduler {
    pub cfg: Config,
    clock: Arc<dyn Clock>,
    store: Store,
    tracker: Arc<dyn Tracker>,
    worker: Arc<dyn Worker>,
    workspace: Arc<dyn Workspace>,
    projector: Arc<dyn Projector>,
    running: HashMap<String, Running>,
    /// Latest issue snapshot seen for each id, for display and routing checks.
    seen: HashMap<String, Issue>,
    /// Consecutive `Continue` verdicts with no tracker state change, per issue.
    no_progress: HashMap<String, u32>,
    ticks: u64,
    last_error: Option<String>,
}

impl Scheduler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        clock: Arc<dyn Clock>,
        store: Store,
        tracker: Arc<dyn Tracker>,
        worker: Arc<dyn Worker>,
        workspace: Arc<dyn Workspace>,
        projector: Arc<dyn Projector>,
    ) -> Self {
        Self {
            cfg,
            clock,
            store,
            tracker,
            worker,
            workspace,
            projector,
            running: HashMap::new(),
            seen: HashMap::new(),
            no_progress: HashMap::new(),
            ticks: 0,
            last_error: None,
        }
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    // ---- tick ----------------------------------------------------------------

    pub fn tick(&mut self) -> anyhow::Result<()> {
        self.ticks += 1;
        self.last_error = None;

        // Unconditional: in-flight runs are reconciled even when config is broken.
        self.harvest_finished()?;
        self.detect_stalls()?;
        self.refresh_running()?;

        if let Err(e) = self.cfg.preflight() {
            self.last_error = Some(format!("preflight: {e}"));
            tracing::error!(error = %e, "dispatch preflight failed; skipping dispatch this tick");
            self.publish()?;
            return Ok(());
        }

        self.dispatch_due_retries()?;
        self.dispatch_new()?;
        self.publish()?;
        Ok(())
    }

    // ---- reconciliation ------------------------------------------------------

    fn harvest_finished(&mut self) -> anyhow::Result<()> {
        let done: Vec<(String, Outcome)> = self
            .running
            .iter()
            .filter_map(|(id, r)| r.handle.finished().map(|o| (id.clone(), o)))
            .collect();

        for (issue_id, outcome) in done {
            let r = self.running.remove(&issue_id).expect("just listed");
            let p = r.handle.progress();
            self.store.finish_run(
                self.clock.as_ref(),
                &r.run_id,
                outcome.label(),
                p.turns,
                p.in_tok,
                p.out_tok,
            )?;
            self.store.add_turns(&issue_id, p.turns)?;
            self.apply_outcome(&issue_id, &r, outcome)?;
        }
        Ok(())
    }

    fn apply_outcome(
        &mut self,
        issue_id: &str,
        r: &Running,
        outcome: Outcome,
    ) -> anyhow::Result<()> {
        match outcome {
            // Done and Blocked both release the claim and then park the issue in whatever state
            // the tracker currently shows. Releasing alone would leave an issue that is still
            // sitting in an active state looking eligible, and the next tick would pick it
            // straight back up — the same runaway as the continuation path, by another route.
            Outcome::Done => {
                tracing::info!(issue_id, identifier = %r.issue.identifier, "run completed: done");
                self.no_progress.remove(issue_id);
                self.store.release(self.clock.as_ref(), issue_id)?;
                self.park_here(issue_id, r)?;
            }
            Outcome::Blocked { why } => {
                tracing::info!(issue_id, identifier = %r.issue.identifier, why, "run blocked; parking");
                self.no_progress.remove(issue_id);
                self.store.release(self.clock.as_ref(), issue_id)?;
                self.park_here(issue_id, r)?;
            }
            Outcome::Continue { why } => {
                let st = self.store.get(issue_id)?;
                let turns = st.as_ref().map(|s| s.cumulative_turns).unwrap_or(0);

                // The global brake the spec lacks: `max_turns` there is per session, and the
                // continuation loop starts a fresh session every time, so nothing bounds the
                // total.
                if turns >= self.cfg.agent.max_turns_per_issue {
                    tracing::warn!(
                        issue_id,
                        turns,
                        budget = self.cfg.agent.max_turns_per_issue,
                        "turn budget exhausted; quarantining instead of continuing"
                    );
                    self.store.record_failure(
                        self.clock.as_ref(),
                        issue_id,
                        ErrorClass::ConfigInvalid,
                        "turn budget exhausted",
                        self.cfg.agent.quarantine_after_identical,
                    )?;
                    return Ok(());
                }

                // Did the tracker state actually move while that run executed?
                let now_state = self.seen.get(issue_id).map(|i| i.state_key()).unwrap_or_default();
                let moved = now_state != r.state_at_start.trim().to_lowercase();
                let streak = if moved {
                    self.no_progress.remove(issue_id);
                    0
                } else {
                    let e = self.no_progress.entry(issue_id.to_string()).or_insert(0);
                    *e += 1;
                    *e - 1
                };

                let delay = retry::continuation_delay_ms(streak, self.cfg.polling.interval_ms);
                let due = Wall(self.clock.wall().0 + delay as i64);
                tracing::info!(issue_id, why, delay_ms = delay, moved, "run continues");
                self.store.schedule_retry(
                    self.clock.as_ref(),
                    issue_id,
                    due,
                    0,
                    &format!("continuation: {why}"),
                )?;
            }
            Outcome::Failed { class, msg } => {
                tracing::warn!(issue_id, class = class.as_str(), msg, "run failed");

                // A run that never took a turn established nothing worth resuming into — and if
                // it was resuming, its target is the likeliest reason it produced nothing at
                // all: the CLI answers a name it no longer holds with "No conversation found
                // with session ID" and exits. Dropping the name here is what keeps a dead
                // session from failing every retry identically, straight into quarantine.
                if r.handle.progress().turns == 0 {
                    tracing::info!(issue_id, "run took no turns; dropping its session name");
                    self.store.set_session(self.clock.as_ref(), issue_id, None)?;
                }

                let quarantined = self.store.record_failure(
                    self.clock.as_ref(),
                    issue_id,
                    class,
                    &msg,
                    self.cfg.agent.quarantine_after_identical,
                )?;
                if quarantined {
                    tracing::error!(
                        issue_id,
                        class = class.as_str(),
                        retryable = class.retryable(),
                        "quarantined; no further dispatch until cleared"
                    );
                    self.no_progress.remove(issue_id);
                } else {
                    let attempt = self.store.get(issue_id)?.map(|s| s.attempt).unwrap_or(1);
                    let delay = retry::backoff_ms(attempt, self.cfg.agent.max_retry_backoff_ms);
                    let due = Wall(self.clock.wall().0 + delay as i64);
                    self.store.schedule_retry(self.clock.as_ref(), issue_id, due, attempt, &msg)?;
                }
            }
        }
        Ok(())
    }

    /// Record the state we last finished this issue in, so it is not immediately re-dispatched.
    fn park_here(&self, issue_id: &str, r: &Running) -> anyhow::Result<()> {
        let state =
            self.seen.get(issue_id).map(|i| i.state_key()).unwrap_or_else(|| r.issue.state_key());
        self.store.park(self.clock.as_ref(), issue_id, &state)?;
        Ok(())
    }

    fn detect_stalls(&mut self) -> anyhow::Result<()> {
        let limit = self.cfg.agent.stall_timeout_ms;
        if limit == 0 {
            return Ok(()); // disabled
        }
        let now = self.clock.mono();

        // Refresh progress and note who has gone quiet.
        let mut stalled = Vec::new();
        for (id, r) in self.running.iter_mut() {
            let p = r.handle.progress();
            if p != r.last_progress {
                r.last_progress = p;
                r.last_progress_at = now;
            } else if now.saturating_since(r.last_progress_at) > limit {
                stalled.push(id.clone());
            }
        }

        for id in stalled {
            tracing::warn!(issue_id = %id, limit_ms = limit, "stalled; terminating");
            self.terminate(&id, false)?;
            self.store.record_failure(
                self.clock.as_ref(),
                &id,
                ErrorClass::Stall,
                "no agent output within stall timeout",
                self.cfg.agent.quarantine_after_identical,
            )?;
            if self.store.get(&id)?.map(|s| s.is_quarantined()) != Some(true) {
                let attempt = self.store.get(&id)?.map(|s| s.attempt).unwrap_or(1);
                let delay = retry::backoff_ms(attempt, self.cfg.agent.max_retry_backoff_ms);
                let due = Wall(self.clock.wall().0 + delay as i64);
                self.store.schedule_retry(self.clock.as_ref(), &id, due, attempt, "stalled")?;
            }
        }
        Ok(())
    }

    fn refresh_running(&mut self) -> anyhow::Result<()> {
        let ids: Vec<String> = self.running.keys().cloned().collect();
        if ids.is_empty() {
            return Ok(());
        }

        let refreshed = match self.tracker.by_ids(&ids) {
            Ok(v) => v,
            Err(e) => {
                // Keep workers running; a tracker blip must not cancel real work.
                log_tracker_failure("running-state refresh failed; keeping workers", &e);
                self.last_error = Some(format!("refresh: {e}"));
                return Ok(());
            }
        };

        let mut returned = Vec::new();
        for issue in refreshed {
            returned.push(issue.id.clone());
            self.store.clear_miss(&issue.id)?;
            let key = issue.state_key();
            self.seen.insert(issue.id.clone(), issue.clone());

            if self.cfg.is_terminal(&key) {
                tracing::info!(issue_id = %issue.id, state = %issue.state, "terminal; stopping and cleaning");
                self.terminate(&issue.id, true)?;
                self.store.release(self.clock.as_ref(), &issue.id)?;
            } else if self.cfg.is_active(&key) && self.routable(&issue) {
                if let Some(r) = self.running.get_mut(&issue.id) {
                    r.issue = issue;
                }
            } else {
                tracing::info!(
                    issue_id = %issue.id, state = %issue.state,
                    "no longer active or routable; stopping without cleanup"
                );
                self.terminate(&issue.id, false)?;
                self.store.release(self.clock.as_ref(), &issue.id)?;
            }
        }

        // Omission is ambiguous: genuinely gone, or a filtered query that has not caught up.
        // The spec kills on the first miss; a grace count means one blip cannot destroy
        // in-flight work.
        for id in ids.iter().filter(|i| !returned.contains(i)) {
            let misses = self.store.bump_miss(id)?;
            if misses >= self.cfg.agent.refresh_miss_grace {
                tracing::info!(issue_id = %id, misses, "not visible for consecutive refreshes; stopping");
                self.terminate(id, false)?;
                self.store.release(self.clock.as_ref(), id)?;
            } else {
                tracing::debug!(issue_id = %id, misses, "not visible; within grace");
            }
        }
        Ok(())
    }

    /// Stop a run and, only once it is confirmed stopped, optionally remove its workspace.
    ///
    /// The ordering is the point. The spec says "terminate worker and clean workspace" with no
    /// constraint between the two, which deletes a directory out from under a process that may
    /// still be writing to it.
    fn terminate(&mut self, issue_id: &str, cleanup: bool) -> anyhow::Result<()> {
        let Some(r) = self.running.remove(issue_id) else {
            return Ok(());
        };
        let outcome = r.handle.kill(KILL_GRACE_MS);
        tracing::debug!(issue_id, ?outcome, "worker stopped");

        let p = r.handle.progress();
        self.store.finish_run(
            self.clock.as_ref(),
            &r.run_id,
            "killed",
            p.turns,
            p.in_tok,
            p.out_tok,
        )?;
        self.store.add_turns(issue_id, p.turns)?;

        if cleanup && let Err(e) = self.workspace.remove(issue_id, &r.issue.identifier) {
            tracing::warn!(issue_id, error = %e, "workspace cleanup failed");
        }
        Ok(())
    }

    // ---- dispatch ------------------------------------------------------------

    fn routable(&self, issue: &Issue) -> bool {
        issue.dispatchable
            && self
                .cfg
                .tracker
                .required_labels
                .iter()
                .all(|want| issue.labels.iter().any(|l| l == want))
    }

    fn global_slots(&self) -> usize {
        self.cfg.agent.max_concurrent.saturating_sub(self.running.len())
    }

    fn state_slots(&self, state_key: &str) -> usize {
        let used = self.running.values().filter(|r| r.issue.state_key() == state_key).count();
        self.cfg.state_limit(state_key).saturating_sub(used)
    }

    fn dispatch_due_retries(&mut self) -> anyhow::Result<()> {
        let due = self.store.due_retries(self.clock.wall())?;
        if due.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = due.iter().map(|d| d.issue_id.clone()).collect();

        let refreshed = match self.tracker.by_ids(&ids) {
            Ok(v) => v,
            Err(e) => {
                log_tracker_failure("retry refresh failed", &e);
                self.last_error = Some(format!("retry refresh: {e}"));
                return Ok(());
            }
        };
        let by_id: HashMap<String, Issue> =
            refreshed.into_iter().map(|i| (i.id.clone(), i)).collect();

        for entry in due {
            let Some(issue) = by_id.get(&entry.issue_id) else {
                // Gone from the tracker: release rather than inventing a state for it.
                tracing::info!(issue_id = %entry.issue_id, "retry target not visible; releasing");
                self.store.clear_retry(&entry.issue_id)?;
                self.store.release(self.clock.as_ref(), &entry.issue_id)?;
                continue;
            };
            self.seen.insert(issue.id.clone(), issue.clone());
            let key = issue.state_key();

            if self.cfg.is_terminal(&key) {
                self.store.clear_retry(&issue.id)?;
                if let Err(e) = self.workspace.remove(&issue.id, &issue.identifier) {
                    tracing::warn!(issue_id = %issue.id, error = %e, "cleanup failed");
                }
                self.store.release(self.clock.as_ref(), &issue.id)?;
                continue;
            }
            if !self.cfg.is_active(&key) || !self.routable(issue) {
                self.store.clear_retry(&issue.id)?;
                self.store.release(self.clock.as_ref(), &issue.id)?;
                continue;
            }
            if self.global_slots() == 0 || self.state_slots(&key) == 0 {
                // Leave the entry in place; it is already due and will be retried next tick.
                tracing::debug!(issue_id = %issue.id, "no slots for retry; deferring");
                continue;
            }

            let issue = issue.clone();
            self.store.clear_retry(&issue.id)?;
            self.launch(&issue, entry.attempt)?;
        }
        Ok(())
    }

    fn dispatch_new(&mut self) -> anyhow::Result<()> {
        let candidates = match self.tracker.by_states(&self.cfg.tracker.active_states) {
            Ok(v) => v,
            Err(e) => {
                log_tracker_failure("candidate fetch failed; skipping dispatch", &e);
                self.last_error = Some(format!("candidates: {e}"));
                return Ok(());
            }
        };

        let mut sorted = candidates;
        sorted.sort_by(|a, b| {
            // Providers disagree about what integer priorities mean, so rank only the presence
            // of a priority, then fall back to age. Adapters that want a different order
            // normalise before we see it.
            let ka = (a.priority.is_none(), a.priority.unwrap_or(i32::MAX));
            let kb = (b.priority.is_none(), b.priority.unwrap_or(i32::MAX));
            ka.cmp(&kb)
                .then_with(|| match (a.created_at, b.created_at) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                })
                .then_with(|| a.identifier.cmp(&b.identifier))
        });

        for issue in sorted {
            if self.global_slots() == 0 {
                break;
            }
            self.seen.insert(issue.id.clone(), issue.clone());

            let key = issue.state_key();
            if !self.cfg.is_active(&key) || self.cfg.is_terminal(&key) {
                continue;
            }
            if !self.routable(&issue) {
                continue;
            }
            if self.running.contains_key(&issue.id) {
                continue;
            }
            if self.state_slots(&key) == 0 {
                // Per-state saturation skips this issue but not the rest of the queue.
                continue;
            }

            let st = self.store.ensure(
                self.clock.as_ref(),
                &issue.id,
                &issue.identifier,
                &worktree_key(&issue.id, &issue.identifier),
            )?;
            if st.is_quarantined() {
                continue;
            }
            if st.phase == Phase::RetryQueued {
                continue; // a timer owns it
            }
            if st.cumulative_turns >= self.cfg.agent.max_turns_per_issue {
                continue;
            }
            match st.parked_state.as_deref() {
                // Still sitting where we left it: nothing new to act on.
                Some(parked) if parked == key => continue,
                // The state moved, so the park is stale and the issue is live again.
                Some(_) => self.store.unpark(self.clock.as_ref(), &issue.id)?,
                None => {}
            }

            self.launch(&issue, 0)?;
        }
        Ok(())
    }

    /// Claim, prepare, spawn — in that order.
    ///
    /// The claim commits before the worker exists. The spec spawns first and records the claim
    /// afterwards, leaving a window in which a fast-exiting worker reports against state that
    /// has not been written yet.
    fn launch(&mut self, issue: &Issue, attempt: u32) -> anyhow::Result<()> {
        self.store.ensure(
            self.clock.as_ref(),
            &issue.id,
            &issue.identifier,
            &worktree_key(&issue.id, &issue.identifier),
        )?;

        if !self.store.claim(self.clock.as_ref(), &issue.id)? {
            tracing::debug!(issue_id = %issue.id, "already claimed; not dispatching");
            return Ok(());
        }

        let prepared = match self.workspace.prepare(&issue.id, &issue.identifier) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(issue_id = %issue.id, error = %e, "workspace preparation failed");
                let class = match e {
                    crate::workspace::WorkspaceError::OutsideRoot { .. } => {
                        ErrorClass::WorkspaceOutsideRoot
                    }
                    _ => ErrorClass::WorkspaceIo,
                };
                let quarantined = self.store.record_failure(
                    self.clock.as_ref(),
                    &issue.id,
                    class,
                    "workspace preparation failed",
                    self.cfg.agent.quarantine_after_identical,
                )?;
                if !quarantined {
                    let attempt = self.store.get(&issue.id)?.map(|s| s.attempt).unwrap_or(1);
                    let delay = retry::backoff_ms(attempt, self.cfg.agent.max_retry_backoff_ms);
                    let due = Wall(self.clock.wall().0 + delay as i64);
                    self.store.schedule_retry(
                        self.clock.as_ref(),
                        &issue.id,
                        due,
                        attempt,
                        "workspace error",
                    )?;
                }
                return Ok(());
            }
        };

        let run_id = format!("{}-{}", issue.id, self.clock.wall().0);

        // The conversation is named here, before the process exists, for the same reason the
        // claim is written first: a run that dies early must still leave behind a name its
        // continuation can resume, and the child cannot be the one to record it.
        let session = match self.store.get(&issue.id)?.and_then(|s| s.session_id) {
            Some(id) => Session::Resume(id),
            None => {
                let id = session_id(&issue.id, self.clock.wall().0);
                self.store.set_session(self.clock.as_ref(), &issue.id, Some(&id))?;
                Session::New(id)
            }
        };
        self.store.start_run(self.clock.as_ref(), &run_id, &issue.id, session.id())?;

        let handle = self.worker.spawn(issue, &prepared.path, attempt, &session);
        // The stall clock starts here, after workspace preparation — not at dispatch. Hook or
        // setup time inside its own timeout must not eat the agent's stall budget.
        let now = self.clock.mono();

        // The branch is logged alongside the directory because it, not the directory, is what
        // a reviewer goes looking for once the run is over.
        tracing::info!(
            issue_id = %issue.id,
            identifier = %issue.identifier,
            attempt,
            workspace = %prepared.path.display(),
            branch = prepared.branch.as_deref().unwrap_or("-"),
            session = session.id(),
            resumed = session.is_resume(),
            "dispatched"
        );

        self.running.insert(
            issue.id.clone(),
            Running {
                run_id,
                state_at_start: issue.state.clone(),
                issue: issue.clone(),
                handle,
                started: now,
                workspace: prepared.path,
                last_progress: Progress::default(),
                last_progress_at: now,
            },
        );
        Ok(())
    }

    // ---- observability -------------------------------------------------------

    pub fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let now_mono = self.clock.mono();
        let now_wall = self.clock.wall().0;
        let states = self.store.all()?;
        let retries: HashMap<String, i64> =
            self.store.all_retries()?.into_iter().map(|r| (r.issue_id, r.due_at)).collect();
        let (in_tok, out_tok) = self.store.token_totals()?;

        let mut rows = Vec::new();
        for st in &states {
            let run = self.running.get(&st.issue_id);
            let issue = self.seen.get(&st.issue_id);
            let progress = run.map(|r| r.handle.progress()).unwrap_or_default();

            rows.push(Row {
                issue_id: st.issue_id.clone(),
                identifier: st.identifier.clone(),
                title: issue.map(|i| i.title.clone()).unwrap_or_default(),
                url: issue.and_then(|i| i.url.clone()),
                tracker_state: issue.map(|i| i.state.clone()).unwrap_or_default(),
                phase: st.phase,
                attempt: st.attempt,
                turns: if run.is_some() { progress.turns } else { st.cumulative_turns },
                in_tok: progress.in_tok,
                out_tok: progress.out_tok,
                age_ms: run.map(|r| now_mono.saturating_since(r.started)).unwrap_or(0),
                retry_in_ms: retries.get(&st.issue_id).map(|d| d - now_wall),
                quarantined: st.is_quarantined(),
                last_error: st.last_error.clone(),
                last_event: progress.last_event,
                workspace: run.map(|r| r.workspace.display().to_string()),
            });
        }

        rows.sort_by(|a, b| {
            // Running first, then anything waiting, then the inert rows.
            let rank = |p: Phase| match p {
                Phase::Running => 0,
                Phase::RetryQueued => 1,
                Phase::Queued => 2,
                Phase::Quarantined => 3,
                Phase::Released => 4,
            };
            rank(a.phase).cmp(&rank(b.phase)).then_with(|| a.identifier.cmp(&b.identifier))
        });

        Ok(Snapshot {
            generated_at: now_wall,
            running: self.running.len(),
            limit: self.cfg.agent.max_concurrent,
            retrying: retries.len(),
            quarantined: states.iter().filter(|s| s.is_quarantined()).count(),
            in_tok,
            out_tok,
            ticks: self.ticks,
            last_tick_at: Some(now_wall),
            last_error: self.last_error.clone(),
            rows,
        })
    }

    fn publish(&self) -> anyhow::Result<()> {
        let snap = self.snapshot()?;
        let projected: Vec<ProjectedIssue> = snap
            .rows
            .iter()
            .map(|r| ProjectedIssue {
                issue_id: r.issue_id.clone(),
                identifier: r.identifier.clone(),
                title: r.title.clone(),
                url: r.url.clone(),
                tracker_state: r.tracker_state.clone(),
                phase: r.phase,
                attempt: r.attempt,
                cumulative_turns: r.turns,
                in_tok: r.in_tok,
                out_tok: r.out_tok,
                workspace: r.workspace.clone(),
                retry_due_at: r.retry_in_ms.map(|d| snap.generated_at + d),
                quarantined: r.quarantined,
                last_error: r.last_error.clone(),
                blocked_by: self
                    .seen
                    .get(&r.issue_id)
                    .map(|i| i.blocked_by.clone())
                    .unwrap_or_default(),
            })
            .collect();

        // Best-effort by contract: a projection failure is a lost view, never a lost tick.
        if let Err(e) = self.projector.project(&projected) {
            tracing::warn!(error = %e, "projection failed; continuing");
        }
        Ok(())
    }

    /// Operator action: clear quarantine so the issue becomes dispatchable again.
    pub fn unquarantine(&self, issue_id: &str) -> anyhow::Result<()> {
        self.store.unquarantine(self.clock.as_ref(), issue_id)?;
        Ok(())
    }

    /// Stop every in-flight run before the process exits.
    ///
    /// A worker `RunHandle` outlives the `Scheduler` that spawned it unless something kills it
    /// explicitly — dropping the handle does not stop the underlying process. Without this, a
    /// real worker process (and its supervising threads) becomes orphaned the moment the
    /// orchestrator exits with runs still in flight: caught empirically, not hypothetically —
    /// the first live end-to-end run of the real worker left a `claude` process running after
    /// `cargo run` returned, because nothing had ever called `terminate` on it. No cleanup: the
    /// issue is not terminal, so the workspace is preserved for reuse on the next start, the
    /// same as `detect_stalls`. The claim is released so the issue is dispatchable again too.
    pub fn shutdown(&mut self) -> anyhow::Result<()> {
        self.terminate_all_running()
    }

    fn terminate_all_running(&mut self) -> anyhow::Result<()> {
        let ids: Vec<String> = self.running.keys().cloned().collect();
        for id in ids {
            tracing::info!(issue_id = %id, "stopping in-flight run for shutdown");
            self.terminate(&id, false)?;
            self.store.release(self.clock.as_ref(), &id)?;
        }
        Ok(())
    }
}

impl Drop for Scheduler {
    /// Last-resort safety net for exit paths that skip the explicit `shutdown()` call — an
    /// early return via `?`, a panic unwinding past the normal loop. `drop` cannot propagate a
    /// `Result`, so a failure here is logged, not surfaced; the `is_empty` guard keeps this
    /// silent on the expected path, where `shutdown()` already emptied `running`.
    fn drop(&mut self) {
        if self.running.is_empty() {
            return;
        }
        tracing::warn!("scheduler dropped with runs still in flight; terminating them now");
        if let Err(e) = self.terminate_all_running() {
            tracing::warn!(error = %e, "cleanup on drop failed");
        }
    }
}
