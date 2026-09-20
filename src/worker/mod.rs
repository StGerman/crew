//! Worker execution.
//!
//! A worker runs one attempt for one issue and reports an explicit [`Outcome`]. The spec infers
//! "maybe continue" from a clean process exit and re-dispatches on a 1s timer, which is how it
//! runs away; here the verdict is data, and only `Continue` earns another dispatch.

pub mod claude;
pub mod fake;

use std::path::PathBuf;
use std::sync::Arc;

use crate::model::{Issue, Outcome};

/// Where this run's host-side tool broker is, when there is one.
///
/// Carries a path rather than a URL because the token inside it is a secret: a `--mcp-config`
/// file (mode 0600, written and owned by the broker) keeps it out of `argv`, where any process
/// on the host could read it out of `ps`. The worker never parses this — it hands the path to
/// the CLI and names the tools in its prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEndpoint {
    /// MCP server name. Tools reach the agent as `mcp__<server>__<tool>`.
    pub server: String,
    /// A `--mcp-config` file carrying this run's own endpoint and bearer token.
    pub config_path: PathBuf,
    /// Tool names, for the prompt. The agent will not use a tool it was not told about.
    pub tools: Vec<String>,
}

impl ToolEndpoint {
    /// The name the agent actually sees, which is not the bare tool name.
    pub fn qualified(&self, tool: &str) -> String {
        format!("mcp__{}__{}", self.server, tool)
    }
}

/// Progress reported while a run is in flight. Drives stall detection and the dashboard.
///
/// Two of these fields answer different questions and must not be merged back into one.
/// `events` is a liveness signal: it exists so that this struct compares unequal between two
/// scheduler ticks whenever the child did anything at all, which is exactly what
/// `detect_stalls` checks. `tokens` is a cost figure, and it is absent until the run's terminal
/// `result` event supplies one. The first version of this type had per-event token counters
/// serving both jobs, and they served the second badly (see [`TokenUsage`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub turns: u32,
    /// Stream events observed so far, of every type — a tool result counts as much as a turn.
    /// A working agent inside a long tool call is not silent, and this is what says so.
    pub events: u64,
    /// The run's token totals, once it has reported them. `None` while the run is in flight and
    /// `None` forever for a run that ended without a `result` event: killed, crashed, or cut off
    /// by the session turn budget. An honest absence, not a zero.
    pub tokens: Option<TokenUsage>,
    pub last_event: Option<String>,
}

/// Token totals for one run, as reported by the agent CLI itself in its terminal `result` event.
///
/// Taken from there and nowhere else. Summing the `usage` block of each streamed `assistant`
/// event looked equivalent and was not, in both directions: the CLI emits one `assistant` event
/// per content block, each carrying the whole turn's usage, so a thinking-then-text turn is
/// counted twice; and the per-event `output_tokens` is a streaming placeholder that reads `1`
/// for a full paragraph. The first live dispatch recorded ten million input tokens and four
/// hundred output tokens over eighty-three turns that way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Prompt-side tokens billed for the run: fresh input plus cache creation plus cache reads.
    /// One figure rather than three because the dashboard has one column; the split is in the
    /// CLI's own transcript if a cost breakdown is ever needed.
    pub input: u64,
    pub output: u64,
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
    /// `tools` is `None` when the broker is unavailable. That is a degrade, not an error: the
    /// run proceeds without tracker tools rather than failing, so a broker that cannot bind
    /// costs the agent a capability and nothing else.
    fn spawn(
        &self,
        issue: &Issue,
        workspace: &std::path::Path,
        attempt: u32,
        session: &Session,
        tools: Option<&ToolEndpoint>,
    ) -> Arc<dyn RunHandle>;
}
