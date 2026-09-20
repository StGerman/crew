//! The tracker *write* seam.
//!
//! Deliberately not on [`Tracker`](crate::tracker::Tracker). That trait is a read kernel and
//! stays one: a scheduler that could mutate tickets would be a scheduler whose decisions are
//! entangled with its side effects, and every fake would have to grow a write surface no
//! scheduler test needs. Ticket mutations belong to the agent, through the broker, which is
//! the only caller of this trait.
//!
//! Every method takes the dispatch id as its *first* argument and the broker fills it in from
//! the calling run. Nothing on this trait lets a caller name an issue it was not handed.

use crate::tracker::TrackerError;

/// Ticket mutations the broker exposes to an agent. Three, and adding a fourth should be a
/// deliberate decision: each one is an authority handed to a model.
pub trait TrackerWrites: Send + Sync {
    /// Post a comment. Returns a human-readable reference (a URL where the provider gives one).
    fn comment(&self, issue_id: &str, body: &str) -> Result<String, TrackerError>;

    /// Move the issue to `state`, which the broker has already checked against the states the
    /// operator configured. Providers without native states map this onto whatever convention
    /// their adapter documents.
    fn set_state(&self, issue_id: &str, state: &str) -> Result<String, TrackerError>;

    /// Associate a pull request with the issue.
    fn link_pr(&self, issue_id: &str, url: &str) -> Result<String, TrackerError>;
}
