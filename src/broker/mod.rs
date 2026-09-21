//! Host-side tool broker: the agent asks, the orchestrator writes.
//!
//! The [`Tracker`](crate::tracker::Tracker) trait is a read kernel on purpose, but an agent
//! still needs to comment, move its ticket and link a PR. Handing it a token to do that would
//! put a credential inside a process running model-authored code. Instead the credential stays
//! here: the agent calls an MCP tool, this module performs the write, the agent receives the
//! result.
//!
//! ## What this actually buys, and what it does not
//!
//! This was measured on the dev machine rather than assumed, because the answer inverted the
//! premise the slice was planned on.
//!
//! The worker's [`DEFAULT_ENV_ALLOWLIST`] keeps every tracker credential out of the child's
//! environment, and that part holds. The open question was whether an agent could reach a
//! credential by another route anyway — `gh`'s stored token under `~/.config/gh`, or git's
//! credential helper via `~/.gitconfig` — and therefore whether the allowlist should stop
//! passing `HOME` through.
//!
//! It should not, because it would not help. On macOS `gh auth token` **succeeds with `HOME`
//! unset**: the token lives in the login keychain, which is keyed to the user session, not to
//! a path under `$HOME`. The same is true of this machine's `claude` OAuth credential — it has
//! no `~/.claude/.credentials.json` at all, and authenticates fine with `HOME` scrubbed. An
//! environment allowlist cannot take away a credential that was never in the environment or
//! the home directory; only a real sandbox (a separate uid, a container, a seatbelt profile)
//! could, and that is a different and much larger change than this slice.
//!
//! So, stating it plainly rather than implying otherwise:
//!
//! > **The broker is scoped convenience plus an audit trail. It is not an isolation boundary.**
//! > A dispatched agent on this platform can still comment, push and close issues as the
//! > operator, through the ambient keychain, without any token appearing in its environment.
//! > What the broker adds is a *sanctioned* path that is scoped to one issue, rate limited and
//! > logged — so the agent has no reason to reach for the ambient one, and a reviewer can see
//! > every write it made through the front door.
//!
//! The honest security property this slice delivers is therefore narrower than "the worker
//! never holds a credential", and is the second of the two options the issue offered. It is
//! recorded here so the next reader does not have to re-derive it — and
//! [`WorkerConfig::env_allowlist`](crate::config::WorkerConfig::env_allowlist) exists for an
//! operator who wants to tighten the environment half regardless.
//!
//! ## Scoping
//!
//! Every session is bound to exactly one issue, and **no tool takes an issue id**. The agent
//! cannot name a target: the broker resolves it from the token the request arrived with. A
//! call that tries to pass one anyway is refused and audited rather than quietly ignored —
//! quietly ignoring it would make an attempt to act on another ticket indistinguishable from a
//! well-formed call, in exactly the log a reviewer would go to.
//!
//! ## Transport
//!
//! MCP over HTTP, on loopback, one listener for the process, with a per-run bearer token in
//! the URL path. See [`server`] for the wire details and why this is hand-rolled rather than
//! built on `rmcp`. The same transport code also serves the operator's [ops
//! tools](crate::api::mcp), but on a listener of their own: those tools are scoped to the whole
//! daemon, and the address a worker is handed must never be one that answers them.

pub mod fake;
pub mod server;
pub mod writes;

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::clock::Clock;
use crate::model::Issue;
use crate::worker::ToolEndpoint;
pub use server::McpService;
pub use writes::TrackerWrites;

/// The MCP server name. Tools reach the agent as `mcp__symphony__<tool>`.
pub const SERVER_NAME: &str = "symphony";

pub const TOOL_COMMENT: &str = "comment";
pub const TOOL_SET_STATE: &str = "set_state";
pub const TOOL_LINK_PR: &str = "link_pr";

/// Argument names that would mean "act on a different ticket". None of them is a parameter of
/// any tool here; they are listed so a call carrying one can be refused with a message that
/// says *why* rather than a generic schema complaint.
const ISSUE_SELECTOR_KEYS: &[&str] =
    &["issue", "issue_id", "issue_key", "issueId", "id", "identifier", "number", "repo", "owner"];

/// Audit entries kept in memory for the dashboard and for tests. The durable copy is the
/// `tracing` event emitted alongside each one — the store is explicitly a droppable cache, and
/// an audit trail you are allowed to lose is not much of an audit trail.
const MAX_AUDIT: usize = 512;

/// Bounded so a crashing or hostile agent cannot turn one comment into a memory problem.
const MAX_ARG_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ToolError {
    /// The call was rejected before any tracker write was attempted.
    #[error("{0}")]
    Refused(String),
    #[error("rate limit reached: {0}")]
    RateLimited(String),
    #[error("tracker write failed: {0}")]
    Tracker(String),
}

impl ToolError {
    fn audit_outcome(&self) -> AuditOutcome {
        match self {
            ToolError::Refused(_) => AuditOutcome::Refused,
            ToolError::RateLimited(_) => AuditOutcome::RateLimited,
            ToolError::Tracker(_) => AuditOutcome::Failed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    Ok,
    Refused,
    RateLimited,
    Failed,
}

impl AuditOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditOutcome::Ok => "ok",
            AuditOutcome::Refused => "refused",
            AuditOutcome::RateLimited => "rate_limited",
            AuditOutcome::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub at: i64,
    pub issue_id: String,
    pub identifier: String,
    pub run_id: String,
    pub tool: String,
    pub outcome: AuditOutcome,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct BrokerLimits {
    /// Calls one dispatched run may make. Bounds a single runaway agent.
    pub max_calls_per_run: u32,
    /// Calls one issue may accumulate across *all* its runs. The per-run cap alone does not
    /// bound the continuation loop, because each continuation is a fresh run with a fresh
    /// allowance — the same gap `max_turns_per_issue` exists to close for turns.
    pub max_calls_per_issue: u32,
}

impl Default for BrokerLimits {
    fn default() -> Self {
        Self { max_calls_per_run: 20, max_calls_per_issue: 100 }
    }
}

struct SessionState {
    issue_id: String,
    identifier: String,
    run_id: String,
    calls: u32,
}

#[derive(Default)]
struct Inner {
    /// Keyed by token. A token is the whole authority: holding one means "I am the run
    /// dispatched for this issue".
    sessions: HashMap<String, SessionState>,
    calls_by_issue: HashMap<String, u32>,
    audit: VecDeque<AuditEntry>,
}

pub struct Broker {
    writes: Arc<dyn TrackerWrites>,
    clock: Arc<dyn Clock>,
    limits: BrokerLimits,
    /// States `set_state` will accept, normalised. Sourced from the operator's configured
    /// active + terminal states, so an agent cannot invent one the scheduler does not know.
    allowed_states: Vec<String>,
    addr: SocketAddr,
    config_dir: PathBuf,
    inner: Mutex<Inner>,
}

impl Broker {
    pub fn new(
        writes: Arc<dyn TrackerWrites>,
        clock: Arc<dyn Clock>,
        limits: BrokerLimits,
        allowed_states: Vec<String>,
        addr: SocketAddr,
        config_dir: impl Into<PathBuf>,
    ) -> std::io::Result<Self> {
        let config_dir = config_dir.into();
        std::fs::create_dir_all(&config_dir)?;
        // 0700: the directory holds per-run bearer tokens. Their filenames are hashes rather
        // than the tokens themselves, but there is no reason for the listing to be readable.
        restrict(&config_dir, 0o700)?;
        Ok(Self {
            writes,
            clock,
            limits,
            allowed_states,
            addr,
            config_dir,
            inner: Mutex::new(Inner::default()),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Open a session for one dispatched run and write the `--mcp-config` the worker will pass.
    ///
    /// The returned guard is the session's lifetime: dropping it revokes the token and removes
    /// the config file, so a run that ends — cleanly, killed, or stalled — cannot leave behind
    /// a credential that still authorises writes.
    pub fn open(self: &Arc<Self>, issue: &Issue, run_id: &str) -> std::io::Result<BrokerSession> {
        let token = random_token()?;
        let url = format!("http://{}{PATH_PREFIX}{token}", self.addr);

        // Named by a hash of the token rather than the token, so the filename does not leak
        // the secret to anything that can list the directory.
        let stem: String = blake3::hash(token.as_bytes()).to_hex().chars().take(16).collect();
        let config_path = self.config_dir.join(format!("{stem}.json"));
        let config = json!({
            "mcpServers": { SERVER_NAME: { "type": "http", "url": url } }
        });
        std::fs::write(&config_path, serde_json::to_vec(&config)?)?;
        restrict(&config_path, 0o600)?;

        self.inner.lock().unwrap().sessions.insert(
            token.clone(),
            SessionState {
                issue_id: issue.id.clone(),
                identifier: issue.identifier.clone(),
                run_id: run_id.to_string(),
                calls: 0,
            },
        );

        tracing::debug!(
            issue_id = %issue.id, run_id, config = %config_path.display(),
            "broker session opened"
        );

        Ok(BrokerSession {
            broker: Arc::clone(self),
            token,
            endpoint: ToolEndpoint {
                server: SERVER_NAME.to_string(),
                config_path: config_path.clone(),
                tools: vec![
                    TOOL_COMMENT.to_string(),
                    TOOL_SET_STATE.to_string(),
                    TOOL_LINK_PR.to_string(),
                ],
            },
            config_path,
        })
    }

    fn close(&self, token: &str, config_path: &Path) {
        let removed = self.inner.lock().unwrap().sessions.remove(token);
        if let Some(s) = removed {
            tracing::debug!(issue_id = %s.issue_id, run_id = %s.run_id, calls = s.calls, "broker session closed");
        }
        // Best effort: a config file we cannot delete is litter, not a live authority — the
        // token it names was just revoked above.
        if let Err(e) = std::fs::remove_file(config_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %config_path.display(), error = %e, "removing broker config failed");
        }
    }

    /// The tool list, as `tools/list` returns it.
    pub fn tools_json(&self) -> Value {
        let states = self.allowed_states.join(", ");
        json!([
            {
                "name": TOOL_COMMENT,
                "description": "Post a comment on the issue you were dispatched for. The \
                                orchestrator performs the write; you cannot choose which issue \
                                it lands on.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "body": { "type": "string", "description": "Markdown comment body." }
                    },
                    "required": ["body"],
                    "additionalProperties": false
                }
            },
            {
                "name": TOOL_SET_STATE,
                "description": format!(
                    "Move the issue you were dispatched for to a new workflow state. Allowed \
                     states: {states}. Moving it to a terminal state ends your own run, so do \
                     it last."
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "state": {
                            "type": "string",
                            "enum": self.allowed_states,
                            "description": "Target state."
                        }
                    },
                    "required": ["state"],
                    "additionalProperties": false
                }
            },
            {
                "name": TOOL_LINK_PR,
                "description": "Link a pull request to the issue you were dispatched for.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "Pull request URL." }
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }
            }
        ])
    }

    /// Perform one tool call on behalf of whoever holds `token`.
    ///
    /// Note the shape: the issue is read out of the session, never out of `args`. That is the
    /// security property this module exists for, and it is why the signature has no issue
    /// parameter for a caller to get wrong.
    pub fn call(&self, token: &str, tool: &str, args: &Value) -> Result<String, ToolError> {
        let Some((issue_id, identifier, run_id)) = self.session_of(token) else {
            // No session to attribute this to, so it is audited against the token's absence.
            // Reaching here means a stale config file, a revoked run still trying, or a forged
            // token — all three are worth a line.
            let e = ToolError::Refused(
                "unknown or expired broker session; this run is no longer authorised to write"
                    .into(),
            );
            tracing::warn!(tool, "broker call with an unknown token refused");
            self.record("<unknown>", "<unknown>", "<unknown>", tool, Err(&e));
            return Err(e);
        };

        let result = self.dispatch(token, &issue_id, tool, args);
        self.record(&issue_id, &identifier, &run_id, tool, result.as_ref());
        result
    }

    fn dispatch(
        &self,
        token: &str,
        issue_id: &str,
        tool: &str,
        args: &Value,
    ) -> Result<String, ToolError> {
        let arg = self.validate(tool, args)?;

        // Counted before the write, not after: a loop of *failing* writes is still a loop, and
        // a budget only spent on successes would not bound it.
        self.charge(token, issue_id)?;

        let out = match tool {
            TOOL_COMMENT => self.writes.comment(issue_id, &arg),
            TOOL_SET_STATE => self.writes.set_state(issue_id, &arg),
            TOOL_LINK_PR => self.writes.link_pr(issue_id, &arg),
            _ => unreachable!("validate rejects unknown tools"),
        };
        out.map_err(|e| ToolError::Tracker(e.to_string()))
    }

    /// Checks the tool exists, that its one required argument is present and usable, and that
    /// nothing else was passed. Returns the argument value.
    fn validate(&self, tool: &str, args: &Value) -> Result<String, ToolError> {
        let key = match tool {
            TOOL_COMMENT => "body",
            TOOL_SET_STATE => "state",
            TOOL_LINK_PR => "url",
            other => {
                return Err(ToolError::Refused(format!("unknown tool: {other}")));
            }
        };

        let obj = args
            .as_object()
            .ok_or_else(|| ToolError::Refused("arguments must be an object".to_string()))?;

        // Unknown keys are refused rather than ignored. `_meta` is the one exception: MCP
        // clients attach it as protocol plumbing, and it says nothing about intent.
        let unknown: Vec<&str> =
            obj.keys().map(String::as_str).filter(|k| *k != key && *k != "_meta").collect();
        if !unknown.is_empty() {
            let selector: Vec<&str> = unknown
                .iter()
                .copied()
                .filter(|k| ISSUE_SELECTOR_KEYS.iter().any(|s| s.eq_ignore_ascii_case(k)))
                .collect();
            if !selector.is_empty() {
                return Err(ToolError::Refused(format!(
                    "refused: {} is scoped to the issue this run was dispatched for and cannot \
                     be redirected; drop {} and call it again",
                    tool,
                    selector.join(", ")
                )));
            }
            return Err(ToolError::Refused(format!(
                "unexpected argument(s) for {tool}: {}",
                unknown.join(", ")
            )));
        }

        let raw = obj
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Refused(format!("{tool} requires a string `{key}`")))?;
        let value = raw.trim();
        if value.is_empty() {
            return Err(ToolError::Refused(format!("{tool}'s `{key}` must not be empty")));
        }
        if value.len() > MAX_ARG_BYTES {
            return Err(ToolError::Refused(format!(
                "{tool}'s `{key}` is {} bytes, over the {MAX_ARG_BYTES} byte limit",
                value.len()
            )));
        }

        match tool {
            TOOL_SET_STATE => {
                let want = value.trim().to_lowercase();
                if !self.allowed_states.contains(&want) {
                    return Err(ToolError::Refused(format!(
                        "state `{value}` is not one this orchestrator is configured for; \
                         allowed: {}",
                        self.allowed_states.join(", ")
                    )));
                }
                Ok(want)
            }
            TOOL_LINK_PR => {
                if !(value.starts_with("https://") || value.starts_with("http://")) {
                    return Err(ToolError::Refused(
                        "link_pr's `url` must be an http(s) URL".to_string(),
                    ));
                }
                Ok(value.to_string())
            }
            _ => Ok(value.to_string()),
        }
    }

    /// Spend one unit of both budgets, or refuse.
    fn charge(&self, token: &str, issue_id: &str) -> Result<(), ToolError> {
        let mut g = self.inner.lock().unwrap();

        let issue_calls = *g.calls_by_issue.get(issue_id).unwrap_or(&0);
        if issue_calls >= self.limits.max_calls_per_issue {
            return Err(ToolError::RateLimited(format!(
                "this issue has used its budget of {} broker calls across all its runs",
                self.limits.max_calls_per_issue
            )));
        }
        let Some(session) = g.sessions.get(token) else {
            return Err(ToolError::Refused("broker session closed mid-call".into()));
        };
        if session.calls >= self.limits.max_calls_per_run {
            return Err(ToolError::RateLimited(format!(
                "this run has used its budget of {} broker calls",
                self.limits.max_calls_per_run
            )));
        }

        g.sessions.get_mut(token).expect("checked above").calls += 1;
        *g.calls_by_issue.entry(issue_id.to_string()).or_insert(0) += 1;
        Ok(())
    }

    fn session_of(&self, token: &str) -> Option<(String, String, String)> {
        let g = self.inner.lock().unwrap();
        g.sessions.get(token).map(|s| (s.issue_id.clone(), s.identifier.clone(), s.run_id.clone()))
    }

    fn record(
        &self,
        issue_id: &str,
        identifier: &str,
        run_id: &str,
        tool: &str,
        result: Result<&String, &ToolError>,
    ) {
        let (outcome, detail) = match result {
            Ok(d) => (AuditOutcome::Ok, truncate(d)),
            Err(e) => (e.audit_outcome(), truncate(&e.to_string())),
        };

        match outcome {
            AuditOutcome::Ok => {
                tracing::info!(issue_id, identifier, run_id, tool, detail, "broker call")
            }
            _ => tracing::warn!(
                issue_id,
                identifier,
                run_id,
                tool,
                outcome = outcome.as_str(),
                detail,
                "broker call not performed"
            ),
        }

        let entry = AuditEntry {
            at: self.clock.wall().0,
            issue_id: issue_id.to_string(),
            identifier: identifier.to_string(),
            run_id: run_id.to_string(),
            tool: tool.to_string(),
            outcome,
            detail,
        };
        let mut g = self.inner.lock().unwrap();
        if g.audit.len() == MAX_AUDIT {
            g.audit.pop_front();
        }
        g.audit.push_back(entry);
    }

    /// Every call the broker has seen this process, oldest first.
    pub fn audit(&self) -> Vec<AuditEntry> {
        self.inner.lock().unwrap().audit.iter().cloned().collect()
    }

    pub fn open_sessions(&self) -> usize {
        self.inner.lock().unwrap().sessions.len()
    }
}

/// Every session URL is `/mcp/<token>`, so the token is the request path with this prefix
/// removed. Anything else — a bare `/mcp`, a different prefix — resolves to no session and is
/// refused the same way a forged token is.
const PATH_PREFIX: &str = "/mcp/";

impl McpService for Broker {
    fn name(&self) -> &str {
        SERVER_NAME
    }

    fn tools(&self, _path: &str) -> Value {
        self.tools_json()
    }

    fn call(&self, path: &str, tool: &str, args: &Value) -> Result<String, String> {
        let token = path.strip_prefix(PATH_PREFIX).unwrap_or("");
        Broker::call(self, token, tool, args).map_err(|e| e.to_string())
    }
}

/// One dispatched run's authority to write, and the file that carries it to the worker.
///
/// Deliberately an RAII guard rather than a `close()` the scheduler must remember: the
/// scheduler already has several paths that end a run — a clean verdict, a stall, a terminal
/// ticket, shutdown, an unwinding `Drop` — and a revocation that any one of them could forget
/// is a revocation that silently does not happen. Holding it in the run's own record means the
/// token dies exactly when the run is dropped, on every path, including the ones not yet
/// written.
pub struct BrokerSession {
    broker: Arc<Broker>,
    token: String,
    endpoint: ToolEndpoint,
    config_path: PathBuf,
}

impl BrokerSession {
    pub fn endpoint(&self) -> &ToolEndpoint {
        &self.endpoint
    }
}

impl Drop for BrokerSession {
    fn drop(&mut self) {
        self.broker.close(&self.token, &self.config_path);
    }
}

impl Drop for Broker {
    /// Remove the config directory, which every session has already emptied of its own file.
    /// `remove_dir` rather than `remove_dir_all` on purpose: if anything is still in there, a
    /// session did not clean up after itself, and leaving the evidence beats destroying it.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.config_dir);
    }
}

/// 32 bytes of kernel entropy, hex encoded.
///
/// Not derived from the run id and clock like [`crate::model::session_id`] is: that one only
/// has to be unique, this one has to be unguessable, and every input a derivation could use
/// here is something an agent already knows.
fn random_token() -> std::io::Result<String> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

fn truncate(s: &str) -> String {
    s.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeWrites, Write};
    use super::*;
    use crate::clock::FakeClock;
    use crate::tracker::TrackerError;

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "symphony-broker-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn issue(id: &str) -> Issue {
        Issue {
            id: id.into(),
            identifier: format!("#{id}"),
            title: "t".into(),
            body: None,
            state: "In Progress".into(),
            priority: None,
            url: None,
            labels: vec![],
            dispatchable: true,
            created_at: None,
            native_ref: None,
            blocked_by: vec![],
        }
    }

    fn broker_with(tag: &str, limits: BrokerLimits) -> (Arc<Broker>, Arc<FakeWrites>) {
        let writes = Arc::new(FakeWrites::new());
        let w: Arc<dyn TrackerWrites> = writes.clone();
        let b = Broker::new(
            w,
            Arc::new(FakeClock::new()),
            limits,
            vec!["in progress".into(), "done".into()],
            "127.0.0.1:1".parse().unwrap(),
            tmp_dir(tag),
        )
        .unwrap();
        (Arc::new(b), writes)
    }

    fn broker(tag: &str) -> (Arc<Broker>, Arc<FakeWrites>) {
        broker_with(tag, BrokerLimits::default())
    }

    #[test]
    fn a_comment_call_causes_exactly_one_tracker_write_performed_by_the_orchestrator() {
        let (b, writes) = broker("one-write");
        let s = b.open(&issue("o/r#1"), "run-1").unwrap();
        let token = s.token.clone();

        b.call(&token, TOOL_COMMENT, &json!({ "body": "progress update" })).unwrap();

        // Exactly one, and against the issue the *session* names — the call never said which.
        assert_eq!(
            writes.writes(),
            vec![Write::Comment { issue_id: "o/r#1".into(), body: "progress update".into() }]
        );

        let audit = b.audit();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].outcome, AuditOutcome::Ok);
        assert_eq!(audit[0].issue_id, "o/r#1");
        assert_eq!(audit[0].run_id, "run-1");
    }

    #[test]
    fn a_call_naming_a_different_issue_is_refused_and_the_refusal_is_audited() {
        let (b, writes) = broker("wrong-issue");
        let s = b.open(&issue("o/r#1"), "run-1").unwrap();

        let err = b
            .call(&s.token, TOOL_COMMENT, &json!({ "body": "hi", "issue_id": "o/r#99" }))
            .unwrap_err();

        assert!(matches!(err, ToolError::Refused(_)), "got {err:?}");
        assert!(
            err.to_string().contains("issue_id"),
            "the refusal must name what was rejected: {err}"
        );
        assert_eq!(writes.count(), 0, "a redirected call must not reach the tracker at all");

        let audit = b.audit();
        assert_eq!(audit.len(), 1, "the attempt is logged, not silently dropped");
        assert_eq!(audit[0].outcome, AuditOutcome::Refused);
        assert_eq!(audit[0].issue_id, "o/r#1", "audited against the run's real issue");
    }

    #[test]
    fn no_tool_accepts_an_issue_id_in_its_schema() {
        // The structural half of the same property: a schema that grew an issue parameter
        // would make the runtime refusal above unreachable, and this catches that at the
        // source rather than one call shape at a time.
        let (b, _) = broker("schema");
        for tool in b.tools_json().as_array().unwrap() {
            let props = tool.pointer("/inputSchema/properties").unwrap().as_object().unwrap();
            for key in props.keys() {
                assert!(
                    !ISSUE_SELECTOR_KEYS.iter().any(|s| s.eq_ignore_ascii_case(key)),
                    "{} exposes `{key}`, which would let a model pick its own target",
                    tool["name"]
                );
            }
            assert_eq!(
                tool.pointer("/inputSchema/additionalProperties"),
                Some(&json!(false)),
                "{} must not accept free-form extras",
                tool["name"]
            );
        }
    }

    #[test]
    fn a_call_bearing_an_unknown_token_is_refused_and_audited() {
        let (b, writes) = broker("bad-token");
        let _s = b.open(&issue("o/r#1"), "run-1").unwrap();

        let err = b.call("not-a-real-token", TOOL_COMMENT, &json!({ "body": "hi" })).unwrap_err();

        assert!(matches!(err, ToolError::Refused(_)), "got {err:?}");
        assert_eq!(writes.count(), 0);
        assert_eq!(b.audit().len(), 1);
        assert_eq!(b.audit()[0].outcome, AuditOutcome::Refused);
    }

    #[test]
    fn a_runs_call_budget_is_spent_by_failures_too_so_a_loop_cannot_outrun_it() {
        // The budget has to count attempts, not successes: an agent looping on a write that
        // keeps failing is exactly the runaway being bounded, and it produces no successes to
        // count.
        let (b, writes) =
            broker_with("budget", BrokerLimits { max_calls_per_run: 3, max_calls_per_issue: 100 });
        let s = b.open(&issue("o/r#1"), "run-1").unwrap();
        writes.fail_with(Some(TrackerError::Status("500".into())));

        for _ in 0..3 {
            let e = b.call(&s.token, TOOL_COMMENT, &json!({ "body": "retry" })).unwrap_err();
            assert!(matches!(e, ToolError::Tracker(_)), "got {e:?}");
        }
        let e = b.call(&s.token, TOOL_COMMENT, &json!({ "body": "retry" })).unwrap_err();
        assert!(matches!(e, ToolError::RateLimited(_)), "got {e:?}");
    }

    #[test]
    fn a_continuation_cannot_refresh_the_budget_by_opening_a_new_session() {
        // Per-run alone would be no bound at all: the continuation loop starts a fresh run
        // every time, which is the same hole `max_turns_per_issue` closes for turns.
        let (b, writes) = broker_with(
            "per-issue",
            BrokerLimits { max_calls_per_run: 10, max_calls_per_issue: 2 },
        );

        for run in 0..3 {
            let s = b.open(&issue("o/r#1"), &format!("run-{run}")).unwrap();
            let _ = b.call(&s.token, TOOL_COMMENT, &json!({ "body": "again" }));
            drop(s);
        }

        assert_eq!(writes.count(), 2, "the third run must not get a fresh allowance");
        assert!(b.audit().iter().any(|e| e.outcome == AuditOutcome::RateLimited));
    }

    #[test]
    fn a_tracker_failure_is_reported_to_the_agent_rather_than_raised_as_a_run_failure() {
        let (b, writes) = broker("tracker-fail");
        let s = b.open(&issue("o/r#1"), "run-1").unwrap();
        writes.fail_with(Some(TrackerError::RateLimited));

        let err = b.call(&s.token, TOOL_COMMENT, &json!({ "body": "hi" })).unwrap_err();

        // The distinction that matters: a failed *write* is a Tracker error the agent can read
        // and work around, not a refusal and not something that ends the run.
        assert!(matches!(err, ToolError::Tracker(_)), "got {err:?}");
        assert_eq!(b.audit()[0].outcome, AuditOutcome::Failed);
    }

    #[test]
    fn set_state_refuses_a_state_the_operator_never_configured() {
        let (b, writes) = broker("bad-state");
        let s = b.open(&issue("o/r#1"), "run-1").unwrap();

        let err = b.call(&s.token, TOOL_SET_STATE, &json!({ "state": "shipped" })).unwrap_err();
        assert!(matches!(err, ToolError::Refused(_)), "got {err:?}");
        assert_eq!(writes.count(), 0);

        // And a configured one still works, so the check is a filter rather than a wall.
        b.call(&s.token, TOOL_SET_STATE, &json!({ "state": "Done" })).unwrap();
        assert_eq!(
            writes.writes(),
            vec![Write::SetState { issue_id: "o/r#1".into(), state: "done".into() }],
            "the state reaches the adapter normalised, as the scheduler spells it"
        );
    }

    #[test]
    fn link_pr_requires_a_url() {
        let (b, writes) = broker("link");
        let s = b.open(&issue("o/r#1"), "run-1").unwrap();

        assert!(b.call(&s.token, TOOL_LINK_PR, &json!({ "url": "not a url" })).is_err());
        assert_eq!(writes.count(), 0);

        b.call(&s.token, TOOL_LINK_PR, &json!({ "url": "https://x/pull/1" })).unwrap();
        assert_eq!(writes.count(), 1);
    }

    #[test]
    fn dropping_a_session_revokes_its_token_and_removes_its_config_file() {
        // This is what makes the RAII guard load-bearing: a run that ended must not leave a
        // token behind that still authorises writes, on any of the paths that end one.
        let (b, writes) = broker("revoke");
        let s = b.open(&issue("o/r#1"), "run-1").unwrap();
        let token = s.token.clone();
        let path = s.endpoint().config_path.clone();

        assert!(path.exists(), "the worker needs a config file to point --mcp-config at");
        assert_eq!(b.open_sessions(), 1);

        drop(s);

        assert!(!path.exists(), "the config file outlived the run it belonged to");
        assert_eq!(b.open_sessions(), 0);
        let err = b.call(&token, TOOL_COMMENT, &json!({ "body": "after the run" })).unwrap_err();
        assert!(matches!(err, ToolError::Refused(_)), "got {err:?}");
        assert_eq!(writes.count(), 0);
    }

    #[test]
    fn two_sessions_get_different_tokens_and_cannot_reach_each_others_issues() {
        let (b, writes) = broker("two");
        let a = b.open(&issue("o/r#1"), "run-a").unwrap();
        let c = b.open(&issue("o/r#2"), "run-b").unwrap();
        assert_ne!(a.token, c.token);

        b.call(&a.token, TOOL_COMMENT, &json!({ "body": "for one" })).unwrap();
        b.call(&c.token, TOOL_COMMENT, &json!({ "body": "for two" })).unwrap();

        let w = writes.writes();
        assert_eq!(w[0].issue_id(), "o/r#1");
        assert_eq!(w[1].issue_id(), "o/r#2");
    }

    #[test]
    fn the_config_file_is_not_readable_by_anyone_but_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let (b, _) = broker("perms");
        let s = b.open(&issue("o/r#1"), "run-1").unwrap();
        let mode = std::fs::metadata(&s.endpoint().config_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the file carries a bearer token; mode was {mode:o}");
    }
}
