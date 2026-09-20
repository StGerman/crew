//! Domain model.
//!
//! Deliberate deviations from the Symphony spec, each fixing a defect found in review:
//!
//! * [`Outcome`] replaces "the worker process exited normally, so maybe there is more to do".
//!   The spec re-dispatches on a 1s timer forever; here only [`Outcome::Continue`] re-dispatches.
//! * [`ErrorClass`] is partitioned into retryable and permanent. The spec has no permanent
//!   class, so a template typo retries every 5 minutes for eternity.
//! * [`worktree_key`] hashes the *dispatch id*, not the identifier. The spec hashes the
//!   identifier and only when sanitisation alters it, which does nothing for two distinct
//!   issues that genuinely share an identifier.

use serde::{Deserialize, Serialize};

/// A normalized work item. Adapters map provider payloads onto this; the scheduler never
/// inspects provider data directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// Opaque dispatch identity. May be a project-item id rather than a ticket id.
    pub id: String,
    /// Human-readable key, e.g. `MT-649`. Used for display and workspace naming.
    pub identifier: String,
    pub title: String,
    /// Free-text description, when the provider has one. The only field of the task the
    /// worker's prompt has any real content from beyond the title — an adapter that can
    /// populate it and doesn't is handing the agent a ticket with no description.
    pub body: Option<String>,
    pub state: String,
    pub priority: Option<i32>,
    pub url: Option<String>,
    pub labels: Vec<String>,
    /// Adapter-derived eligibility for provider rules the scheduler cannot infer
    /// (assignment, board membership, blocker semantics).
    pub dispatchable: bool,
    pub created_at: Option<i64>,
    /// Non-secret provider identifiers, preserved opaquely for tool context.
    pub native_ref: Option<serde_json::Value>,
    /// Dispatch ids of issues blocking this one. Projected into the task store as `blockedBy`.
    pub blocked_by: Vec<String>,
}

impl Issue {
    /// Trimmed + lowercased state, for scheduler comparison only. Provider spelling is
    /// preserved in `state` for display.
    pub fn state_key(&self) -> String {
        self.state.trim().to_lowercase()
    }
}

/// The orchestrator's claim state for an issue. Distinct from tracker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Phase {
    Queued,
    Running,
    RetryQueued,
    Quarantined,
    #[default]
    Released,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Queued => "queued",
            Phase::Running => "running",
            Phase::RetryQueued => "retry",
            Phase::Quarantined => "quarantine",
            Phase::Released => "released",
        }
    }
}

/// What a worker reports when its run ends. An explicit verdict, never inferred from exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Reached a handoff state. Release the claim.
    Done,
    /// Real work remains. Eligible for re-dispatch, subject to the turn budget.
    Continue {
        why: String,
    },
    /// Needs a human. Release the claim without retrying.
    Blocked {
        why: String,
    },
    Failed {
        class: ErrorClass,
        msg: String,
    },
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Continue { .. } => "continue",
            Outcome::Blocked { .. } => "blocked",
            Outcome::Failed { .. } => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorClass {
    // Retryable: transient conditions that a later attempt may survive.
    TrackerRequest,
    TrackerStatus,
    RateLimited,
    TurnTimeout,
    Stall,
    AgentCrash,
    WorkspaceIo,
    // Permanent: deterministic failures that will recur identically. Retrying is a hot loop.
    TemplateRender,
    ConfigInvalid,
    AgentNotFound,
    WorkspaceOutsideRoot,
    AuthFailed,
}

impl ErrorClass {
    /// Exhaustive by construction: adding a variant without classifying it fails the build.
    pub fn retryable(self) -> bool {
        match self {
            ErrorClass::TrackerRequest
            | ErrorClass::TrackerStatus
            | ErrorClass::RateLimited
            | ErrorClass::TurnTimeout
            | ErrorClass::Stall
            | ErrorClass::AgentCrash
            | ErrorClass::WorkspaceIo => true,

            ErrorClass::TemplateRender
            | ErrorClass::ConfigInvalid
            | ErrorClass::AgentNotFound
            | ErrorClass::WorkspaceOutsideRoot
            | ErrorClass::AuthFailed => false,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::TrackerRequest => "tracker_request",
            ErrorClass::TrackerStatus => "tracker_status",
            ErrorClass::RateLimited => "rate_limited",
            ErrorClass::TurnTimeout => "turn_timeout",
            ErrorClass::Stall => "stall",
            ErrorClass::AgentCrash => "agent_crash",
            ErrorClass::WorkspaceIo => "workspace_io",
            ErrorClass::TemplateRender => "template_render",
            ErrorClass::ConfigInvalid => "config_invalid",
            ErrorClass::AgentNotFound => "agent_not_found",
            ErrorClass::WorkspaceOutsideRoot => "workspace_outside_root",
            ErrorClass::AuthFailed => "auth_failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "tracker_request" => ErrorClass::TrackerRequest,
            "tracker_status" => ErrorClass::TrackerStatus,
            "rate_limited" => ErrorClass::RateLimited,
            "turn_timeout" => ErrorClass::TurnTimeout,
            "stall" => ErrorClass::Stall,
            "agent_crash" => ErrorClass::AgentCrash,
            "workspace_io" => ErrorClass::WorkspaceIo,
            "template_render" => ErrorClass::TemplateRender,
            "config_invalid" => ErrorClass::ConfigInvalid,
            "agent_not_found" => ErrorClass::AgentNotFound,
            "workspace_outside_root" => ErrorClass::WorkspaceOutsideRoot,
            "auth_failed" => ErrorClass::AuthFailed,
            _ => return None,
        })
    }
}

/// Directory name for an issue's workspace.
///
/// Sanitises the identifier for display value, then appends a hash of the *dispatch id* so two
/// issues can never share a directory — whether because their identifiers sanitise to the same
/// text, or because the adapter handed us a duplicate identifier outright.
pub fn worktree_key(issue_id: &str, identifier: &str) -> String {
    let sanitized: String = identifier
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();

    let digest = blake3::hash(issue_id.as_bytes());
    let suffix: String = digest.to_hex().chars().take(12).collect();

    let stem = if sanitized.is_empty() { "issue" } else { sanitized.as_str() };
    format!("{stem}-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_class_is_classified_and_round_trips() {
        let all = [
            ErrorClass::TrackerRequest,
            ErrorClass::TrackerStatus,
            ErrorClass::RateLimited,
            ErrorClass::TurnTimeout,
            ErrorClass::Stall,
            ErrorClass::AgentCrash,
            ErrorClass::WorkspaceIo,
            ErrorClass::TemplateRender,
            ErrorClass::ConfigInvalid,
            ErrorClass::AgentNotFound,
            ErrorClass::WorkspaceOutsideRoot,
            ErrorClass::AuthFailed,
        ];
        for c in all {
            let _ = c.retryable();
            assert_eq!(ErrorClass::parse(c.as_str()), Some(c), "round trip {}", c.as_str());
        }
        assert!(!ErrorClass::TemplateRender.retryable());
        assert!(ErrorClass::RateLimited.retryable());
    }

    #[test]
    fn identifiers_sharing_sanitized_text_get_distinct_keys() {
        // `MT/649` and `MT_649` both sanitise to `MT_649`.
        let a = worktree_key("id-a", "MT/649");
        let b = worktree_key("id-b", "MT_649");
        assert_ne!(a, b);
        assert!(a.starts_with("MT_649-"));
        assert!(b.starts_with("MT_649-"));
    }

    #[test]
    fn duplicate_identifiers_still_get_distinct_keys() {
        // The case the spec's identifier-hash cannot catch: same identifier, different issues.
        assert_ne!(worktree_key("id-a", "MT-649"), worktree_key("id-b", "MT-649"));
    }

    #[test]
    fn key_is_stable_and_contains_only_safe_characters() {
        let k = worktree_key("id-a", "MT-649");
        assert_eq!(k, worktree_key("id-a", "MT-649"));
        assert!(
            k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "unsafe characters in {k}"
        );
    }

    #[test]
    fn traversal_attempts_are_neutralised() {
        // Sanitisation yields `.._.._etc_passwd`: dots survive, separators do not. A dot run is
        // harmless inside a single path component — traversal needs a separator — so the
        // invariants that matter are "no separator" and "not exactly . or ..".
        for identifier in ["../../etc/passwd", "..", ".", "a\\b", "with space/slash"] {
            let k = worktree_key("id-a", identifier);
            assert!(!k.contains('/'), "separator survived in {k}");
            assert!(!k.contains('\\'), "separator survived in {k}");
            assert!(k != "." && k != "..", "resolved to a relative directory: {k}");
        }
    }
}
