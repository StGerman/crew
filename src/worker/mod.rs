//! Worker execution.
//!
//! A worker runs one attempt for one issue and reports an explicit [`Outcome`]. The spec infers
//! "maybe continue" from a clean process exit and re-dispatches on a 1s timer, which is how it
//! runs away; here the verdict is data, and only `Continue` earns another dispatch.

pub mod claude;
pub mod fake;

use std::sync::Arc;

use crate::model::{Issue, Outcome};

/// Progress reported while a run is in flight. Drives stall detection and the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub turns: u32,
    pub in_tok: u64,
    pub out_tok: u64,
    pub last_event: Option<String>,
}

/// Which conversation an attempt runs in.
///
/// The scheduler names it before the process exists, the same ordering as claim-before-spawn:
/// a worker that reported its own id back afterwards would leave a window in which a run that
/// died early could never be resumed, because nothing outside it ever learned the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    /// Start a fresh conversation under this id.
    New(String),
    /// Continue the conversation already recorded under this id.
    Resume(String),
}

impl Session {
    pub fn id(&self) -> &str {
        match self {
            Self::New(id) | Self::Resume(id) => id,
        }
    }

    pub fn is_resume(&self) -> bool {
        matches!(self, Self::Resume(_))
    }
}

/// A run in flight. Dropping the handle does not stop the work — call [`RunHandle::kill`].
pub trait RunHandle: Send + Sync {
    fn progress(&self) -> Progress;
    /// True once the run has produced a verdict.
    fn finished(&self) -> Option<Outcome>;
    /// Request termination and wait, bounded, for the run to actually stop.
    ///
    /// Must not return until the run is confirmed stopped — the caller deletes the workspace
    /// next, and the spec's failure to order these is how a live agent gets its directory
    /// removed mid-write.
    fn kill(&self, grace_ms: u64) -> KillResult;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillResult {
    /// Stopped within the grace period.
    Stopped,
    /// Did not stop in time and was forced. Still safe to clean up after.
    Forced,
    /// Already finished before the request.
    AlreadyDone,
}

pub trait Worker: Send + Sync {
    fn spawn(
        &self,
        issue: &Issue,
        workspace: &std::path::Path,
        attempt: u32,
        session: &Session,
    ) -> Arc<dyn RunHandle>;
}
