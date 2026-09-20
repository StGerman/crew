//! Simulated worker.
//!
//! Progress is a pure function of elapsed clock time, so a test advances [`FakeClock`] and the
//! simulation advances with it — no threads, no sleeps, no flake. Under `SystemClock` the same
//! code animates the dashboard in real time.
//!
//! [`FakeClock`]: crate::clock::FakeClock

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::{KillResult, Progress, RunHandle, Worker};
use crate::clock::{Clock, Mono};
use crate::model::{ErrorClass, Issue, Outcome};

#[derive(Debug, Clone)]
pub struct Script {
    pub duration_ms: u64,
    pub turns: u32,
    pub in_tok_per_turn: u64,
    pub out_tok_per_turn: u64,
    pub outcome: Outcome,
    /// Stop emitting progress at this point while never finishing — a stalled agent.
    pub silent_after_ms: Option<u64>,
}

impl Script {
    pub fn succeeds_in(ms: u64) -> Self {
        Self {
            duration_ms: ms,
            turns: 3,
            in_tok_per_turn: 400,
            out_tok_per_turn: 250,
            outcome: Outcome::Done,
            silent_after_ms: None,
        }
    }

    pub fn with_outcome(mut self, o: Outcome) -> Self {
        self.outcome = o;
        self
    }

    pub fn stalls_after(ms: u64) -> Self {
        Self {
            duration_ms: u64::MAX, // never completes on its own
            turns: 2,
            in_tok_per_turn: 300,
            out_tok_per_turn: 120,
            outcome: Outcome::Failed { class: ErrorClass::Stall, msg: "no output".into() },
            silent_after_ms: Some(ms),
        }
    }
}

impl Default for Script {
    fn default() -> Self {
        Self::succeeds_in(4_000)
    }
}

pub struct FakeWorker {
    clock: Arc<dyn Clock>,
    scripts: Mutex<HashMap<String, Script>>,
    default_script: Mutex<Script>,
}

impl FakeWorker {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            scripts: Mutex::new(HashMap::new()),
            default_script: Mutex::new(Script::default()),
        }
    }

    /// Script a specific issue. Later spawns for the same issue reuse it unless replaced.
    pub fn script(&self, issue_id: &str, s: Script) {
        self.scripts.lock().unwrap().insert(issue_id.to_string(), s);
    }

    pub fn set_default(&self, s: Script) {
        *self.default_script.lock().unwrap() = s;
    }
}

impl Worker for FakeWorker {
    fn spawn(&self, issue: &Issue, _workspace: &Path, _attempt: u32) -> Arc<dyn RunHandle> {
        let script = self
            .scripts
            .lock()
            .unwrap()
            .get(&issue.id)
            .cloned()
            .unwrap_or_else(|| self.default_script.lock().unwrap().clone());

        Arc::new(FakeRun {
            clock: Arc::clone(&self.clock),
            started: self.clock.mono(),
            script,
            killed: AtomicBool::new(false),
            forced: AtomicBool::new(false),
        })
    }
}

struct FakeRun {
    clock: Arc<dyn Clock>,
    started: Mono,
    script: Script,
    killed: AtomicBool,
    forced: AtomicBool,
}

impl FakeRun {
    /// Elapsed time the simulation can observe. Past `silent_after_ms` the clock keeps moving
    /// but the run stops noticing, which is exactly what a stalled agent looks like from outside.
    fn visible_elapsed(&self) -> u64 {
        let raw = self.clock.mono().saturating_since(self.started);
        match self.script.silent_after_ms {
            Some(cut) => raw.min(cut),
            None => raw,
        }
    }
}

impl RunHandle for FakeRun {
    fn progress(&self) -> Progress {
        let elapsed = self.visible_elapsed();
        let turns = if self.script.duration_ms == u64::MAX {
            // Stalling script: ramp turns over the silent window, then hold.
            let window = self.script.silent_after_ms.unwrap_or(1).max(1);
            ((elapsed * self.script.turns as u64) / window).min(self.script.turns as u64) as u32
        } else {
            let d = self.script.duration_ms.max(1);
            (((elapsed * self.script.turns as u64) / d) + 1).min(self.script.turns as u64) as u32
        };

        Progress {
            turns,
            in_tok: turns as u64 * self.script.in_tok_per_turn,
            out_tok: turns as u64 * self.script.out_tok_per_turn,
            last_event: Some(if turns == 0 { "starting" } else { "turn_completed" }.into()),
        }
    }

    fn finished(&self) -> Option<Outcome> {
        if self.killed.load(Ordering::SeqCst) {
            return Some(Outcome::Failed {
                class: ErrorClass::AgentCrash,
                msg: if self.forced.load(Ordering::SeqCst) {
                    "terminated (forced)".into()
                } else {
                    "terminated".into()
                },
            });
        }
        let elapsed = self.clock.mono().saturating_since(self.started);
        (elapsed >= self.script.duration_ms).then(|| self.script.outcome.clone())
    }

    fn kill(&self, _grace_ms: u64) -> KillResult {
        if self.finished().is_some() && !self.killed.load(Ordering::SeqCst) {
            return KillResult::AlreadyDone;
        }
        if self.killed.swap(true, Ordering::SeqCst) {
            return KillResult::AlreadyDone;
        }
        // A stalling run ignores the polite request, mirroring a wedged process.
        if self.script.silent_after_ms.is_some() {
            self.forced.store(true, Ordering::SeqCst);
            KillResult::Forced
        } else {
            KillResult::Stopped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;

    fn issue() -> Issue {
        Issue {
            id: "iss-1".into(),
            identifier: "MT-1".into(),
            title: "t".into(),
            body: None,
            state: "In Progress".into(),
            priority: Some(1),
            url: None,
            labels: vec![],
            dispatchable: true,
            created_at: None,
            native_ref: None,
            blocked_by: vec![],
        }
    }

    #[test]
    fn a_run_finishes_only_once_the_clock_reaches_its_duration() {
        let c = Arc::new(FakeClock::new());
        let w = FakeWorker::new(c.clone());
        let h = w.spawn(&issue(), Path::new("/tmp"), 0);

        assert!(h.finished().is_none());
        c.advance_ms(3_999);
        assert!(h.finished().is_none());
        c.advance_ms(1);
        assert_eq!(h.finished(), Some(Outcome::Done));
    }

    #[test]
    fn progress_accumulates_turns_and_tokens_as_time_passes() {
        let c = Arc::new(FakeClock::new());
        let w = FakeWorker::new(c.clone());
        let h = w.spawn(&issue(), Path::new("/tmp"), 0);

        let p0 = h.progress();
        c.advance_ms(4_000);
        let p1 = h.progress();
        assert!(p1.turns > p0.turns);
        assert!(p1.in_tok > p0.in_tok && p1.out_tok > p0.out_tok);
    }

    #[test]
    fn a_stalling_run_freezes_its_progress_but_never_finishes() {
        let c = Arc::new(FakeClock::new());
        let w = FakeWorker::new(c.clone());
        w.script("iss-1", Script::stalls_after(1_000));
        let h = w.spawn(&issue(), Path::new("/tmp"), 0);

        c.advance_ms(1_000);
        let frozen = h.progress();
        c.advance_ms(600_000);
        assert_eq!(h.progress(), frozen, "a stalled run must stop reporting progress");
        assert!(h.finished().is_none(), "and must never complete on its own");
    }

    #[test]
    fn killing_a_wedged_run_reports_that_force_was_needed() {
        let c = Arc::new(FakeClock::new());
        let w = FakeWorker::new(c.clone());
        w.script("iss-1", Script::stalls_after(1_000));
        let h = w.spawn(&issue(), Path::new("/tmp"), 0);

        c.advance_ms(5_000);
        assert_eq!(h.kill(1_000), KillResult::Forced);
        assert!(matches!(h.finished(), Some(Outcome::Failed { .. })));
    }

    #[test]
    fn killing_an_already_finished_run_is_a_no_op() {
        let c = Arc::new(FakeClock::new());
        let w = FakeWorker::new(c.clone());
        let h = w.spawn(&issue(), Path::new("/tmp"), 0);
        c.advance_ms(4_000);
        assert_eq!(h.kill(1_000), KillResult::AlreadyDone);
        assert_eq!(h.finished(), Some(Outcome::Done), "verdict must not be rewritten");
    }
}
