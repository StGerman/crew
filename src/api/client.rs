//! The other end of [the ops API](super), behind `symphony-cc status`.
//!
//! The API landed before this did, and in the window between the two it was not reached for
//! once — diagnosing a live run meant tailing a block-buffered log file hours behind the
//! daemon, which produced a wrong answer. `curl` would have been right and instant. Nobody
//! typed it because there was nothing to type: a port to remember and a `jq` filter to write
//! is enough friction to send an operator back to the logs. So this is deliberately a client
//! and nothing more — no new server surface, and no second way to reach the scheduler.
//!
//! Two properties it inherits rather than re-decides:
//!
//! * **It reads the published API only.** No `Store`, no `~/.claude/tasks`, no git. The same
//!   rule that keeps the TUI from becoming load-bearing, for the same reason — and the same
//!   consequence, which is that a question this cannot answer is a missing `Snapshot` field
//!   rather than a reason to open the database here. [`crate::sched::Row::branch`] was added
//!   under exactly that rule.
//! * **It shares the wire types with the server.** [`Snapshot`] and [`Row`] are serialised by
//!   one and deserialised by the other, so a field renamed on the scheduler breaks this at
//!   compile time instead of silently rendering a blank column.
//!
//! What this module owns beyond the request is the part an operator actually feels: finding
//! the daemon without being told where it is ([`endpoint`]), and saying something useful when
//! it is not there ([`StatusError`]).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::config::{ApiConfig, DEFAULT_API_BIND};
use crate::sched::{Row, Snapshot};

/// A status query is a question about right now, so it fails fast rather than hanging on a
/// daemon that accepted the connection and then wedged. Generous next to a loopback round
/// trip, short next to an operator's patience.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Where the address came from, so a failure can say which one it tried and why.
///
/// An operator who typoed `api.bind` and one who never enabled the API see the same
/// connection refusal; only this tells them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An explicit `--api`.
    Flag,
    /// An `[api] bind` in this config file.
    Config(PathBuf),
    /// Nothing named one, so [`DEFAULT_API_BIND`].
    Default,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Flag => write!(f, "from --api"),
            Source::Config(p) => write!(f, "from [api] bind in {}", p.display()),
            Source::Default => write!(f, "the default; no --api and no [api] bind in the config"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub addr: String,
    pub source: Source,
}

/// Find the daemon without making the operator remember a port.
///
/// `--api`, then the config's `[api] bind`, then [`DEFAULT_API_BIND`] — the same order of
/// precedence `main` uses when it decides where to *serve*, so the client looks where the
/// daemon was told to listen.
pub fn endpoint(explicit: Option<&str>, config_path: &Path) -> Endpoint {
    if let Some(addr) = explicit.map(str::trim).filter(|a| !a.is_empty()) {
        return Endpoint { addr: addr.to_string(), source: Source::Flag };
    }
    match bind_in(config_path) {
        Some(addr) => Endpoint { addr, source: Source::Config(config_path.to_path_buf()) },
        None => Endpoint { addr: DEFAULT_API_BIND.to_string(), source: Source::Default },
    }
}

/// Read `[api] bind` out of a config, forgivingly.
///
/// Deliberately not [`crate::config::Config::load`]: that runs `preflight`, which gates
/// *dispatch*. A missing `tracker.owner` is a real problem for the daemon and none at all for
/// a client asking what the daemon is doing — refusing to print status over it would be the
/// same class of mistake as validating `api.bind` in preflight. Anything unreadable falls
/// through to the default, where a wrong guess costs one clearly-labelled connection refusal.
fn bind_in(config_path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct JustApi {
        api: ApiConfig,
    }
    let text = std::fs::read_to_string(config_path).ok()?;
    let parsed: JustApi = toml::from_str(&text).ok()?;
    Some(parsed.api.bind)
}

/// Why a status query did not produce a snapshot.
///
/// The split that matters is the first two variants, and it is the one an operator cannot make
/// from a stack trace: nothing accepted the connection, versus something accepted it and said
/// no. They have different fixes and only one of them means "the daemon is down".
#[derive(Debug)]
pub enum StatusError {
    /// Nothing is listening. The daemon is not running — or it is, with its API off, which is
    /// the default and so the likelier of the two.
    NotListening { endpoint: Endpoint },
    /// A daemon answered and refused. Reachable, so this is a question about the request or
    /// the daemon's state, not about whether it is up.
    Refused { endpoint: Endpoint, status: u16, detail: String },
    /// Reachable in principle, but the connection did not complete in time.
    Unreachable { endpoint: Endpoint, why: String },
    /// Something answered that is not this API — a different service on the port, or a version
    /// that no longer speaks this shape.
    Unrecognised { endpoint: Endpoint, why: String },
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatusError::NotListening { endpoint: e } => write!(
                f,
                "nothing is listening on {} ({}).\n\n\
                 Either no daemon is running, or one is running with its ops API off — it is \
                 off by default. Turn it on with `[api] enabled = true` in the config, or \
                 start the daemon with `--api {}` for one run.",
                e.addr, e.source, e.addr
            ),
            StatusError::Refused { endpoint: e, status, detail } => write!(
                f,
                "the daemon on {} ({}) is running and refused the request: HTTP {status}, \
                 {detail}",
                e.addr, e.source
            ),
            StatusError::Unreachable { endpoint: e, why } => write!(
                f,
                "could not reach {} ({}): {why}.\n\n\
                 Something is in the way rather than absent — a firewall, or a daemon that \
                 accepted the connection and then stopped answering.",
                e.addr, e.source
            ),
            StatusError::Unrecognised { endpoint: e, why } => write!(
                f,
                "{} ({}) answered, but not as this ops API: {why}.\n\n\
                 Another service is probably on that port.",
                e.addr, e.source
            ),
        }
    }
}

impl std::error::Error for StatusError {}

/// A daemon's published state, read over the API it publishes it on.
pub struct Client {
    endpoint: Endpoint,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(endpoint: Endpoint) -> Self {
        // The same reason `UreqHttp` does it: a non-2xx has to arrive as an ordinary response,
        // because the body carries the message this client shows the operator. Turned into an
        // `Err` it would be a bare status code.
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .build()
            .into();
        Self { endpoint, agent }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn snapshot(&self) -> Result<Snapshot, StatusError> {
        self.get("/api/v1/snapshot")
    }

    /// One issue by dispatch id or identifier, resolved server-side — this client does not
    /// fetch the whole snapshot and filter, because a duplicated identifier is the API's
    /// `409` to answer and not this one's to guess at.
    pub fn issue(&self, key: &str) -> Result<Row, StatusError> {
        self.get(&format!("/api/v1/issues/{}", encode_segment(key)))
    }

    /// The raw JSON for a path, for `--json`. Kept next to the typed calls so both go through
    /// the same failure classification.
    pub fn raw(&self, path: &str) -> Result<String, StatusError> {
        self.fetch(path).map(|(_, body)| body)
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, StatusError> {
        let (_, body) = self.fetch(path)?;
        serde_json::from_str(&body).map_err(|e| StatusError::Unrecognised {
            endpoint: self.endpoint.clone(),
            why: format!("the response is not a snapshot this version understands ({e})"),
        })
    }

    fn fetch(&self, path: &str) -> Result<(u16, String), StatusError> {
        let url = format!("http://{}{path}", self.endpoint.addr);
        let response = self.agent.get(&url).call().map_err(|e| self.classify(e))?;

        let status = response.status().as_u16();
        let body = response.into_body().read_to_string().map_err(|e| StatusError::Unreachable {
            endpoint: self.endpoint.clone(),
            why: format!("the response head arrived but its body did not ({e})"),
        })?;

        if status == 200 {
            return Ok((status, body));
        }
        Err(StatusError::Refused {
            endpoint: self.endpoint.clone(),
            status,
            detail: message_in(&body),
        })
    }

    /// Turn a transport failure into the distinction an operator needs.
    ///
    /// `ConnectionRefused` is the whole point: on loopback it means the port is closed, which
    /// is "no daemon" and never "busy daemon". `ConnectionFailed` is `ureq`'s fallback for a
    /// connector that produced nothing and no reason, and on a loopback address a refusal is
    /// overwhelmingly what that is, so it lands the same way rather than in a vaguer bucket.
    fn classify(&self, e: ureq::Error) -> StatusError {
        let endpoint = self.endpoint.clone();
        match e {
            ureq::Error::Io(io) if io.kind() == ErrorKind::ConnectionRefused => {
                StatusError::NotListening { endpoint }
            }
            ureq::Error::ConnectionFailed => StatusError::NotListening { endpoint },
            ureq::Error::Timeout(_) => {
                StatusError::Unreachable { endpoint, why: "it did not answer in time".into() }
            }
            ureq::Error::HostNotFound => {
                StatusError::Unreachable { endpoint, why: "the host does not resolve".into() }
            }
            ureq::Error::BadUri(why) => StatusError::Unreachable {
                endpoint,
                why: format!("that is not a usable address ({why})"),
            },
            ureq::Error::Protocol(why) => StatusError::Unrecognised {
                endpoint,
                why: format!("its reply is not valid HTTP ({why})"),
            },
            other => StatusError::Unreachable { endpoint, why: other.to_string() },
        }
    }
}

/// The API's `{"error": ...}` if the body carries one, and the body itself otherwise. A
/// non-JSON error body is still the most informative thing available, so it is shown rather
/// than replaced with a generic line.
fn message_in(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() { "no detail".into() } else { trimmed.to_string() }
        })
}

/// Percent-encode one path segment.
///
/// Identifiers are whatever the tracker hands over, and the server splits on `/` before it
/// decodes — so a `/` left raw here would arrive as a segment boundary and miss the route it
/// was aimed at. Encoding conservatively (anything outside unreserved) keeps `?`, `#` and
/// space out of the target too.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(name: &str, body: &str) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("symphony-client-{}-{name}.toml", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn the_address_comes_from_the_flag_then_the_config_then_the_default() {
        let cfg = write_config("precedence", "[api]\nbind = \"127.0.0.1:9001\"\n");

        let flag = endpoint(Some("127.0.0.1:9999"), &cfg);
        assert_eq!(flag.addr, "127.0.0.1:9999");
        assert_eq!(flag.source, Source::Flag);

        let from_cfg = endpoint(None, &cfg);
        assert_eq!(from_cfg.addr, "127.0.0.1:9001");
        assert_eq!(from_cfg.source, Source::Config(cfg.clone()));

        let missing = endpoint(None, Path::new("/nonexistent/symphony.toml"));
        assert_eq!(missing.addr, DEFAULT_API_BIND);
        assert_eq!(missing.source, Source::Default);

        std::fs::remove_file(&cfg).ok();
    }

    #[test]
    fn a_config_the_daemon_would_reject_still_yields_an_address() {
        // `Config::load` refuses this — no `tracker.kind`, no `active_states`. Preflight gates
        // dispatch, and a client asking what is running has no stake in it; failing here would
        // make an unrelated typo look like the daemon being unreachable.
        let cfg = write_config(
            "unloadable",
            "[tracker]\nkind = \"\"\n[api]\nbind = \"127.0.0.1:9100\"\n",
        );
        assert!(crate::config::Config::load(&cfg).is_err(), "the daemon must still reject this");

        let e = endpoint(None, &cfg);
        assert_eq!(e.addr, "127.0.0.1:9100");
        std::fs::remove_file(&cfg).ok();
    }

    #[test]
    fn an_unparseable_config_falls_back_rather_than_failing() {
        let cfg = write_config("garbage", "this is not toml {{{");
        let e = endpoint(None, &cfg);
        assert_eq!(e.addr, DEFAULT_API_BIND);
        assert_eq!(e.source, Source::Default);
        std::fs::remove_file(&cfg).ok();
    }

    #[test]
    fn a_config_without_an_api_table_reports_the_default_as_the_default() {
        // The address is the same either way; what must not happen is blaming a file that
        // never named one when the connection is refused.
        let cfg = write_config("no-api", "[tracker]\nkind = \"fake\"\n");
        let e = endpoint(None, &cfg);
        assert_eq!(e.addr, DEFAULT_API_BIND);
        assert_eq!(e.source, Source::Default);
        std::fs::remove_file(&cfg).ok();
    }

    #[test]
    fn an_identifier_with_a_separator_cannot_escape_its_path_segment() {
        // The server splits before it decodes, so a raw `/` here would route somewhere else.
        assert_eq!(encode_segment("MT-649"), "MT-649");
        assert_eq!(encode_segment("a/b"), "a%2Fb");
        assert_eq!(encode_segment("a b?c#d"), "a%20b%3Fc%23d");
        assert_eq!(encode_segment("iss_1.2~3"), "iss_1.2~3");
    }

    #[test]
    fn an_error_body_is_quoted_back_whether_or_not_it_is_json() {
        assert_eq!(message_in(r#"{"error":"no such endpoint"}"#), "no such endpoint");
        assert_eq!(message_in("plain text failure"), "plain text failure");
        assert_eq!(message_in("   "), "no detail");
        assert_eq!(message_in(r#"{"other":1}"#), r#"{"other":1}"#);
    }

    #[test]
    fn a_closed_port_reads_as_no_daemon_rather_than_a_refused_request() {
        // The distinction issue #24 asks for, against a real closed port: bind one to learn a
        // number the OS just handed out, then drop it so nothing is there.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap().to_string();
        drop(probe);

        let client = Client::new(Endpoint { addr: addr.clone(), source: Source::Flag });
        match client.snapshot() {
            Err(StatusError::NotListening { .. }) => {}
            other => panic!("a closed port must read as NotListening, got {other:?}"),
        }

        // And the message has to be actionable, not just correct.
        let rendered = client.snapshot().unwrap_err().to_string();
        assert!(rendered.contains(&addr), "the message must name the address tried: {rendered}");
        assert!(rendered.contains("[api] enabled"), "it must name the likeliest fix: {rendered}");
    }
}
