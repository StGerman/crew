//! The code-hosting seam: pull requests, CI verdicts, review requests and review threads.
//!
//! [`Tracker`](crate::tracker::Tracker) reads tickets and [`TrackerWrites`](crate::broker::TrackerWrites)
//! lets the agent write to its own ticket. Neither knows what a pull request is, and neither
//! should: a ticket is where work is *asked for*, a pull request is where it is *delivered*,
//! and on every provider except GitHub those are different systems with different
//! credentials. This trait is the delivery half, and the scheduler drives it directly — the
//! agent never holds it. Opening a pull request, reading CI and recording verdicts on review
//! comments are orchestrator decisions about the agent's output, not things the agent does to
//! its own ticket.
//!
//! **There is no `merge` method, and there must not be one.** Merging stays with a human; the
//! trait's shape is what enforces that, rather than a config flag somebody can flip.
//!
//! Two things a caller must know about the contract:
//!
//! * `request_review` performs the request and nothing else. Whether a reviewer actually
//!   attached is answered by [`Forge::pull_request`] and [`Forge::reviews`] afterwards, and the
//!   scheduler is what asks — because the provider's own answer cannot be trusted here: GitHub
//!   answers a request for a bot reviewer with `200` and attaches nobody (GETT-174120), and the
//!   only way to notice is to look.
//! * `ci_status` reports [`CiStatus::Pending`] for a head with no check runs yet as well as for
//!   one still running. A repository with no CI at all therefore reads as pending forever; the
//!   scheduler bounds that with a timeout rather than this trait guessing.

pub mod fake;
pub mod github;

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, thiserror::Error)]
pub enum ForgeError {
    /// Transient: try again later. A network failure, a 5xx, a rate limit.
    #[error("forge request failed: {0}")]
    Transient(String),
    /// Will not resolve on its own: a bad credential, a 422 on an input that will not change.
    #[error("forge refused: {0}")]
    Permanent(String),
    /// The branch holds nothing the base does not already have — there is no pull request to
    /// open. Distinct from an error so a run that committed nothing reads as "nothing to
    /// deliver" rather than "delivery failed".
    #[error("nothing to deliver: {0}")]
    NothingToDeliver(String),
}

impl ForgeError {
    pub fn retryable(&self) -> bool {
        matches!(self, ForgeError::Transient(_))
    }
}

/// Everything needed to open a pull request. The body is composed by the scheduler from the
/// run record — never by the agent — which is what keeps it honest about what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSpec {
    pub title: String,
    pub body: String,
    /// The branch the work is on.
    pub head: String,
    /// The branch to merge into. `master` normally; another issue's branch for a stacked
    /// pull request.
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub head_sha: String,
    pub base: String,
    /// `open`, `closed` or `merged`. A merged pull request also reads as not open.
    pub state: PrState,
    /// Logins with a review request outstanding. A reviewer who has already posted a review is
    /// no longer in this list — check [`Forge::reviews`] too before concluding nobody attached.
    pub requested_reviewers: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// One submitted review, enough to tell whether a requested reviewer has answered and for
/// which commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub reviewer: String,
    pub commit_sha: String,
    /// The provider's own word for it: `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`.
    pub state: String,
}

/// The CI verdict for one head commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiStatus {
    /// No check has failed yet and at least one has not finished — or none has started.
    Pending,
    Success,
    /// At least one check failed. `failures` is what the agent will be handed.
    Failure {
        failures: Vec<CiFailure>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiFailure {
    /// The check's name, e.g. `fmt + clippy + test`.
    pub name: String,
    pub url: Option<String>,
    /// Whatever the provider can say about *why*: the failed step, a log tail. Bounded by the
    /// implementation, because this lands in a prompt.
    pub detail: String,
}

/// One top-level review comment on a pull request. Replies are not surfaced — a thread is
/// settled or not by its root, and the verdicts this project records are replies to roots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    /// Provider id, stable across polls. The key verdicts are recorded under.
    pub id: String,
    pub author: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub body: String,
    pub url: Option<String>,
}

pub trait Forge: Send + Sync {
    /// Open a pull request for `spec.head` against `spec.base`, or return the one already open
    /// for that head. Idempotent by contract, because the scheduler calls it after every run
    /// that reports done — the second and later calls must find the first call's result.
    fn open_pull_request(&self, spec: &PullRequestSpec) -> Result<PullRequest, ForgeError>;

    /// The pull request as it is now: head, state, outstanding review requests.
    fn pull_request(&self, number: u64) -> Result<PullRequest, ForgeError>;

    /// Ask `reviewer` to review. Performs the request only — see the module doc for why the
    /// caller, not this method, decides whether it worked.
    fn request_review(&self, number: u64, reviewer: &str) -> Result<(), ForgeError>;

    fn reviews(&self, number: u64) -> Result<Vec<Review>, ForgeError>;

    fn ci_status(&self, head_sha: &str) -> Result<CiStatus, ForgeError>;

    /// Top-level review comments, oldest first.
    fn review_comments(&self, number: u64) -> Result<Vec<ReviewComment>, ForgeError>;

    /// Reply on the thread rooted at `comment_id`. This is how a verdict becomes visible to
    /// the reviewer who left the comment.
    fn reply(&self, number: u64, comment_id: &str, body: &str) -> Result<(), ForgeError>;
}

/// What a run leaves on its branch after `Workspace::publish` has pushed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub head_sha: String,
    /// Commit subjects on `head` that `base` does not have, newest first. Empty means the run
    /// committed nothing new — and nothing to open a pull request over.
    pub commits: Vec<String>,
}

/// The git half of delivery: push the branch and describe what is on it. On the
/// [`Workspace`](crate::workspace::Workspace) rather than on [`Forge`] because it is a git
/// operation in the worktree, not a provider API call, and because the plain-directory
/// workspace has to be able to say "no branch here" rather than fake one.
pub trait Publisher: Send + Sync {
    /// Push `branch` from `worktree` to `remote`, and list its commits over `base`.
    fn publish(
        &self,
        worktree: &Path,
        branch: &str,
        remote: &str,
        base: &str,
    ) -> Result<Published, ForgeError>;

    /// Which of `candidates` this branch is stacked on, if any: the candidate that is an
    /// ancestor of `branch`, carries commits `base` does not, and is not itself an ancestor of
    /// another such candidate. `None` when the work sits directly on `base`.
    fn stacked_on(
        &self,
        worktree: &Path,
        branch: &str,
        base: &str,
        candidates: &[String],
    ) -> Result<Option<String>, ForgeError>;
}
