//! The tracker boundary: a read kernel, nothing more.
//!
//! Two operations. Deliberately no generic comment/state/attachment CRUD — those lose provider
//! semantics and the scheduler never needs them. Ticket mutations belong to the agent via
//! host-executed tools (slice 5), not to this trait.

pub mod fake;
pub mod github;

use crate::model::{ErrorClass, Issue};

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

impl TrackerError {
    /// Reuses the scheduler's own retryable/permanent split rather than a second one, so a
    /// tracker failure and a workspace or run failure read the same way in a log line. A
    /// malformed payload is treated as retryable alongside a bad status: in practice it is
    /// almost always a transient truncation or provider hiccup, not a permanent schema break
    /// worth escalating identically to a bad credential.
    pub fn class(&self) -> ErrorClass {
        match self {
            TrackerError::Request(_) => ErrorClass::TrackerRequest,
            TrackerError::Status(_) | TrackerError::Response(_) => ErrorClass::TrackerStatus,
            TrackerError::RateLimited => ErrorClass::RateLimited,
            TrackerError::Auth(_) => ErrorClass::AuthFailed,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_auth_failure_is_permanent() {
        assert!(TrackerError::Request(String::new()).class().retryable());
        assert!(TrackerError::Status(String::new()).class().retryable());
        assert!(TrackerError::Response(String::new()).class().retryable());
        assert!(TrackerError::RateLimited.class().retryable());
        assert!(
            !TrackerError::Auth(String::new()).class().retryable(),
            "a bad credential will not fix itself on retry"
        );
    }
}
