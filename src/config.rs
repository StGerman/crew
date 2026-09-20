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
