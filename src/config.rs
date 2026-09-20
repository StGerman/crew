//! Runtime configuration and dispatch preflight.
//!
//! TOML rather than the spec's YAML-front-matter-in-Markdown. The prompt body belongs in a
//! skill, not wedged into a config file, and a fourth config location in a Claude Code repo
//! is a cost with no payoff.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn d_interval() -> u64 {
    30_000
}
fn d_max_agents() -> usize {
    10
}
fn d_turns_per_session() -> u32 {
    20
}
fn d_turns_per_issue() -> u32 {
    120
}
fn d_backoff_cap() -> u64 {
    300_000
}
fn d_stall() -> u64 {
    300_000
}
fn d_quarantine_after() -> u32 {
    3
}
fn d_miss_grace() -> u32 {
    2
}
fn d_api_bind() -> String {
    "127.0.0.1:8787".to_string()
}
fn d_broker_enabled() -> bool {
    true
}
fn d_calls_per_run() -> u32 {
    20
}
fn d_calls_per_issue() -> u32 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub tracker: TrackerConfig,
    #[serde(default)]
    pub polling: PollingConfig,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub worker: WorkerConfig,
    #[serde(default)]
    pub broker: BrokerConfig,
    #[serde(default)]
    pub api: ApiConfig,
}

/// The ops HTTP surface ([`crate::api`]).
///
/// Off by default, and loopback when on: `POST /refresh` and `POST /unquarantine` control
/// agent execution, so an orchestrator that grows a control plane merely by being upgraded, or
/// that binds `0.0.0.0` because a field was left at a convenient default, is not something an
/// operator asked for.
///
/// Deliberately absent from [`Config::preflight`]: preflight gates *dispatch*, so validating
/// the bind address there would let a typo in a field the scheduler does not use stop the
/// scheduler. The address is parsed once, by [`crate::api::bind`], where a failure costs the
/// API and nothing else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    #[serde(default)]
    pub enabled: bool,
    /// `host:port`. Refused unless the host is loopback or `allow_public` is set.
    #[serde(default = "d_api_bind")]
    pub bind: String,
    /// Permit a non-loopback bind — a deliberate decision to expose the write endpoints to
    /// whatever can reach that interface, which is why it is a separate flag rather than an
    /// inference from the address.
    #[serde(default)]
    pub allow_public: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { enabled: false, bind: d_api_bind(), allow_public: false }
    }
}

/// The host-side tool broker (see [`crate::broker`]).
///
/// On by default, unlike `worker.kind`, because the blast radius is the other way round: the
/// broker's whole purpose is to give the agent a *scoped* way to do what it can already do
/// ambiently, so switching it off does not remove an authority, it removes the audited path to
/// one. An operator who wants no agent-initiated tracker writes at all wants a token without
/// write scope, not `enabled = false`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerConfig {
    #[serde(default = "d_broker_enabled")]
    pub enabled: bool,
    /// Broker calls one dispatched run may make.
    #[serde(default = "d_calls_per_run")]
    pub max_calls_per_run: u32,
    /// Broker calls one issue may accumulate across all of its runs. The per-run cap does not
    /// bound the continuation loop on its own — each continuation is a fresh run with a fresh
    /// allowance — which is the same gap `max_turns_per_issue` closes for turns.
    #[serde(default = "d_calls_per_issue")]
    pub max_calls_per_issue: u32,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            enabled: d_broker_enabled(),
            max_calls_per_run: d_calls_per_run(),
            max_calls_per_issue: d_calls_per_issue(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// `fake` (default) or `claude`. Deliberately independent of `tracker.kind` — a real
    /// tracker with a fake worker is a safe way to watch real dispatch decisions without
    /// spawning real agents; making `claude` the default the moment a real tracker is
    /// configured would turn "point this at a real repo" into "start editing that repo" with
    /// no separate decision in between.
    #[serde(default)]
    pub kind: String,
    /// Only used when `kind = "claude"`. Defaults to `"claude"` — resolved via `PATH`.
    #[serde(default)]
    pub bin: Option<String>,
    /// Replaces
    /// [`DEFAULT_ENV_ALLOWLIST`](crate::worker::claude::DEFAULT_ENV_ALLOWLIST) wholesale when
    /// set — not merged with it, so an operator who names a list gets exactly that list and
    /// nothing arrives by inheritance.
    ///
    /// The reason to reach for it is usually to *add* something deliberate, like
    /// `ANTHROPIC_API_KEY` on an API-key install. Tightening it is a weaker lever than it
    /// looks: see [`crate::broker`] on why removing `HOME` does not take the agent's ambient
    /// credentials away.
    #[serde(default)]
    pub env_allowlist: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrackerConfig {
    /// Selects an adapter: `fake` or `github`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub active_states: Vec<String>,
    #[serde(default)]
    pub terminal_states: Vec<String>,
    #[serde(default)]
    pub required_labels: Vec<String>,
    /// `kind = "github"` only: the repository owner and name to poll. The token comes from
    /// `GITHUB_TOKEN` in the environment, never from this file.
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollingConfig {
    #[serde(default = "d_interval")]
    pub interval_ms: u64,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self { interval_ms: d_interval() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// The git repository worktrees are created from. Defaults to the current directory, which
    /// is the shape dogfooding takes: symphony-cc run from inside the repo it dispatches
    /// against.
    #[serde(default)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "d_max_agents")]
    pub max_concurrent: usize,
    /// Per-state override. Keys are normalised (trim + lowercase) on load.
    #[serde(default)]
    pub max_concurrent_by_state: std::collections::HashMap<String, usize>,
    /// Turns within one worker session. The spec stops here, which is why it runs away.
    #[serde(default = "d_turns_per_session")]
    pub max_turns_per_session: u32,
    /// Turns across *all* sessions for an issue. The global brake the spec lacks entirely.
    #[serde(default = "d_turns_per_issue")]
    pub max_turns_per_issue: u32,
    #[serde(default = "d_backoff_cap")]
    pub max_retry_backoff_ms: u64,
    #[serde(default = "d_stall")]
    pub stall_timeout_ms: u64,
    /// Consecutive identical retryable failures before quarantine.
    #[serde(default = "d_quarantine_after")]
    pub quarantine_after_identical: u32,
    /// Consecutive refresh misses before a running issue is treated as gone. The spec kills on
    /// the first miss, so one eventual-consistency blip destroys in-flight work.
    #[serde(default = "d_miss_grace")]
    pub refresh_miss_grace: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_concurrent: d_max_agents(),
            max_concurrent_by_state: Default::default(),
            max_turns_per_session: d_turns_per_session(),
            max_turns_per_issue: d_turns_per_issue(),
            max_retry_backoff_ms: d_backoff_cap(),
            stall_timeout_ms: d_stall(),
            quarantine_after_identical: d_quarantine_after(),
            refresh_miss_grace: d_miss_grace(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config at {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("cannot parse config at {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Read { path: path.to_path_buf(), source })?;
        let mut cfg: Config = toml::from_str(&text)
            .map_err(|source| ConfigError::Parse { path: path.to_path_buf(), source })?;
        cfg.normalize();
        cfg.preflight()?;
        Ok(cfg)
    }

    /// Lowercase every state used for comparison, so provider spelling never leaks into lookups.
    fn normalize(&mut self) {
        let norm = |v: &Vec<String>| -> Vec<String> {
            v.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect()
        };
        self.tracker.active_states = norm(&self.tracker.active_states);
        self.tracker.terminal_states = norm(&self.tracker.terminal_states);
        self.tracker.required_labels = norm(&self.tracker.required_labels);

        self.agent.max_concurrent_by_state = self
            .agent
            .max_concurrent_by_state
            .iter()
            .map(|(k, v)| (k.trim().to_lowercase(), *v))
            .filter(|(k, v)| !k.is_empty() && *v > 0)
            .collect();
    }

    /// Checks that must hold before the scheduling loop starts, and again before each dispatch.
    pub fn preflight(&self) -> Result<(), ConfigError> {
        if self.tracker.kind.trim().is_empty() {
            return Err(ConfigError::Invalid("tracker.kind is required".into()));
        }
        if self.tracker.kind.trim().eq_ignore_ascii_case("github")
            && (self.tracker.owner.trim().is_empty() || self.tracker.repo.trim().is_empty())
        {
            return Err(ConfigError::Invalid(
                "tracker.owner and tracker.repo are required when tracker.kind = \"github\"".into(),
            ));
        }
        // The spec omits this, so a service with no active states polls forever, dispatches
        // nothing, logs nothing, and looks perfectly healthy.
        if self.tracker.active_states.is_empty() {
            return Err(ConfigError::Invalid("tracker.active_states must be non-empty".into()));
        }
        if self.agent.max_concurrent == 0 {
            return Err(ConfigError::Invalid("agent.max_concurrent must be > 0".into()));
        }
        if self.agent.max_turns_per_session == 0 || self.agent.max_turns_per_issue == 0 {
            return Err(ConfigError::Invalid("turn budgets must be > 0".into()));
        }
        if self.polling.interval_ms == 0 {
            return Err(ConfigError::Invalid("polling.interval_ms must be > 0".into()));
        }
        // A zero cap would not mean "unlimited", it would refuse the agent's first call and
        // read as the broker being broken. Anyone wanting that wants `enabled = false`.
        if self.broker.enabled
            && (self.broker.max_calls_per_run == 0 || self.broker.max_calls_per_issue == 0)
        {
            return Err(ConfigError::Invalid(
                "broker call budgets must be > 0; use broker.enabled = false to disable tools"
                    .into(),
            ));
        }
        // Overlap makes startup cleanup delete a workspace for an issue about to be dispatched.
        let overlap: Vec<_> = self
            .tracker
            .active_states
            .iter()
            .filter(|s| self.tracker.terminal_states.contains(s))
            .cloned()
            .collect();
        if !overlap.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "states appear in both active and terminal: {}",
                overlap.join(", ")
            )));
        }
        Ok(())
    }

    pub fn is_active(&self, state_key: &str) -> bool {
        self.tracker.active_states.iter().any(|s| s == state_key)
    }

    pub fn is_terminal(&self, state_key: &str) -> bool {
        self.tracker.terminal_states.iter().any(|s| s == state_key)
    }

    /// Every state the operator has named, active or terminal. The broker accepts exactly
    /// these for `set_state`, so an agent cannot move its ticket somewhere the scheduler has
    /// no rule for.
    pub fn known_states(&self) -> Vec<String> {
        let mut v = self.tracker.active_states.clone();
        v.extend(self.tracker.terminal_states.iter().cloned());
        v.dedup();
        v
    }

    pub fn state_limit(&self, state_key: &str) -> usize {
        self.agent
            .max_concurrent_by_state
            .get(state_key)
            .copied()
            .unwrap_or(self.agent.max_concurrent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        let mut c = Config {
            tracker: TrackerConfig {
                kind: "fake".into(),
                active_states: vec!["In Progress".into()],
                terminal_states: vec!["Done".into()],
                required_labels: vec![],
                owner: String::new(),
                repo: String::new(),
            },
            polling: Default::default(),
            workspace: Default::default(),
            agent: Default::default(),
            worker: Default::default(),
            broker: Default::default(),
            api: Default::default(),
        };
        c.normalize();
        c
    }

    #[test]
    fn states_are_normalized_for_comparison() {
        let c = base();
        assert!(c.is_active("in progress"));
        assert!(c.is_terminal("done"));
        assert!(!c.is_active("Done"));
    }

    #[test]
    fn empty_active_states_is_rejected() {
        let mut c = base();
        c.tracker.active_states.clear();
        assert!(c.preflight().is_err());
    }

    #[test]
    fn overlapping_active_and_terminal_states_are_rejected() {
        let mut c = base();
        c.tracker.terminal_states.push("in progress".into());
        assert!(c.preflight().is_err());
    }

    #[test]
    fn a_github_tracker_without_owner_and_repo_is_rejected() {
        let mut c = base();
        c.tracker.kind = "github".into();
        assert!(c.preflight().is_err());
        c.tracker.owner = "o".into();
        assert!(c.preflight().is_err(), "repo is still missing");
        c.tracker.repo = "r".into();
        assert!(c.preflight().is_ok());
    }

    #[test]
    fn per_state_limit_falls_back_to_global_and_drops_invalid_entries() {
        let mut c = base();
        c.agent.max_concurrent = 7;
        c.agent.max_concurrent_by_state.insert("  In Progress ".into(), 2);
        c.agent.max_concurrent_by_state.insert("In Review".into(), 0); // invalid, dropped
        c.normalize();
        assert_eq!(c.state_limit("in progress"), 2);
        assert_eq!(c.state_limit("in review"), 7);
        assert_eq!(c.state_limit("anything"), 7);
    }
}
