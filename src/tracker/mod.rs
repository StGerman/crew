//! The tracker boundary: a read kernel, nothing more.
//!
//! Two operations. Deliberately no generic comment/state/attachment CRUD — those lose provider
//! semantics and the scheduler never needs them. Ticket mutations belong to the agent via
//! host-executed tools (slice 5), not to this trait.

pub mod fake;

use crate::model::Issue;

#[derive(Debug, Clone, thiserror::Error)]
pub enum TrackerError {
    #[error("transport failure: {0}")]
    Request(String),
    #[error("non-success response: {0}")]
    Status(String),
    #[error("rate limited")]
    RateLimited,
    #[error("malformed payload: {0}")]
    Response(String),
    #[error("auth failed: {0}")]
    Auth(String),
}

pub trait Tracker: Send + Sync {
    /// Issues currently in any of the given normalized states.
    ///
    /// Includes issues with `dispatchable == false` — the scheduler owns that final filter.
    /// An empty `states` returns empty without a provider request.
    fn by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError>;

    /// Current snapshots for specific dispatch ids, used for reconciliation.
    ///
    /// Full snapshots rather than bare states: labels, routing and dispatchability can all
    /// change while a run is active. Ids no longer visible are omitted — the scheduler treats
    /// omission as "not visible", and applies a grace count before acting on it. A successful
    /// result is complete for that call; partial success must surface as an error instead.
    fn by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError>;
}
