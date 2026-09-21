//! The handoff gate: what stands between an agent saying `Done` and a human being handed the
//! branch.
//!
//! "Agent said done" is not a merge signal. A dispatched run leaves commits on `symphony/<key>`
//! green against the base it forked from and unknown against the base it will actually merge
//! into — and the three defects in issue #21 existed only in that combination, where neither
//! agent could have seen them. So a `Done` verdict is not applied until the run's branch has
//! been rebased onto the configured base *and* the gate commands have passed on the rebased
//! tree, in that order, in the agent's own worktree. The rebase has to come first because a
//! gate run against a stale base answers a question nobody asked.
//!
//! Like the worker, a gate is a process the scheduler supervises rather than a call it makes:
//! `cargo test` in a real worktree runs for minutes, and a tick that blocked on it would stall
//! stall-detection for every other run. [`Gate::start`] returns a handle the scheduler polls;
//! the claim stays held for as long as the handle is open, so nothing can dispatch a second
//! agent onto a worktree that is mid-rebase.
//!
//! The verdict the scheduler derives from the gate is deliberately three-way. A conflict is a
//! human's problem — the branch is restored to where the agent left it and the issue parks
//! `Blocked` naming the paths. A failing command is the agent's — the output goes back to it in
//! the continuation prompt and the verdict is `Continue`. And a run of consecutive failures
//! escalates to `Blocked`, because a gate that can be failed forever is a runaway with a new
//! name.

pub mod fake;
pub mod git;

use std::path::Path;
use std::sync::Arc;

use crate::model::Issue;
use crate::worker::KillResult;

pub use git::GitGate;

/// What the gate found. Produced once, when the handle finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The branch holds nothing the base does not. There is nothing to hand off, so there is
    /// nothing to rebase and nothing worth gating; the run's `Done` stands as it was.
    NoCommits,
    /// Rebased onto the base — or already on top of it — and every command exited zero.
    Passed {
        /// True when the rebase actually moved commits, false when the branch was already
        /// current. For the log; the scheduler treats both the same.
        rebased: bool,
    },
    /// The rebase stopped on conflicts. The rebase was aborted, so the branch is exactly where
    /// the agent left it: green against the old base, and safe for a human to pick up.
    Conflict { paths: Vec<String> },
    /// A step failed for a reason the agent can act on: a command exited non-zero, the rebase
    /// was refused (a dirty tree, most likely), or a command could not be started at all.
    /// `step` names which, and `output` is what it said — bounded, tail-first, because a
    /// failing `cargo test` puts its summary at the end.
    Failed {
        step: String,
        output: String,
        /// Whether the branch is sitting on the base by the time this failed.
        ///
        /// False for every step that runs before the rebase — resolving the base, counting
        /// commits — and for a rebase that was refused and therefore aborted. True only once
        /// the rebase has completed, which is the case where a command failed *on the rebased
        /// tree*. The scheduler needs the distinction because it tells the agent where its work
        /// now sits: saying "the branch has been rebased, fix this on top of it" when nothing
        /// was rebased describes a tree the agent will not find, and an agent that cannot
        /// reconcile the instruction with what it sees tends to report `Done` again unchanged.
        ///
        /// Distinct from [`Verdict::Passed`]'s `rebased`, which answers a different question —
        /// whether the rebase *moved* anything. A branch already on the base is `rebased:
        /// false` there and `on_base: true` here.
        on_base: bool,
    },
}

/// A gate in flight. Dropping the handle does not stop the work — call [`GateHandle::kill`].
pub trait GateHandle: Send + Sync {
    /// The verdict, once there is one.
    fn finished(&self) -> Option<Verdict>;
    /// Which step is running right now, for the dashboard and for the timeout message.
    fn step(&self) -> String;
    /// Stop whatever is running and wait, bounded, for it to actually stop. The same contract
    /// as [`crate::worker::RunHandle::kill`], for the same reason: the caller may delete the
    /// worktree next.
    fn kill(&self, grace_ms: u64) -> KillResult;
}

pub trait Gate: Send + Sync {
    /// Begin gating the run whose worktree is at `workspace`. Never fails: a gate that cannot
    /// even start reports that as a [`Verdict::Failed`] through the handle, so the scheduler
    /// has one path to reason about.
    fn start(&self, issue: &Issue, workspace: &Path) -> Arc<dyn GateHandle>;
}
