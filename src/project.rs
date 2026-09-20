//! Projection of scheduler state into an external, human-readable surface.
//!
//! One-way by contract: the orchestrator writes, and never reads back to make a decision. The
//! intended slice-2 target is `~/.claude/tasks/<session-id>/`, which is durable JSON on disk
//! (`owner`, `status`, `blockedBy` and an arbitrary nested `metadata` object all round-trip) —
//! but it is Claude Code's internal store with no published schema. Keeping the flow one-way
//! means a schema change costs a dashboard, not the scheduler.
//!
//! Slice 1 ships the seam and a no-op, so nothing internal can destabilise the part we are
//! trying to prove correct.

use crate::model::Phase;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedIssue {
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub url: Option<String>,
    pub tracker_state: String,
    pub phase: Phase,
    pub attempt: u32,
    pub cumulative_turns: u32,
    pub in_tok: u64,
    pub out_tok: u64,
    pub workspace: Option<String>,
    pub retry_due_at: Option<i64>,
    pub quarantined: bool,
    pub last_error: Option<String>,
    /// Dispatch ids this issue waits on, rendered as task dependencies.
    pub blocked_by: Vec<String>,
}

pub trait Projector: Send + Sync {
    /// Best-effort. An error here is logged and ignored: this is a view, never a dependency.
    fn project(&self, issues: &[ProjectedIssue]) -> anyhow::Result<()>;
}

pub struct NoopProjector;

impl Projector for NoopProjector {
    fn project(&self, _issues: &[ProjectedIssue]) -> anyhow::Result<()> {
        Ok(())
    }
}
