//! `Tracker` against GitHub Issues.
//!
//! ## The state-mapping problem
//!
//! GitHub Issues have exactly two workflow states: open and closed. This project's scheduler
//! wants an arbitrary set of `active_states` and `terminal_states` — enough to express, for
//! example, a busy "in review" state with its own `max_concurrent_by_state` cap, which
//! `symphony.toml`'s demo config already relies on. Open/closed alone cannot express that.
//!
//! Three ways to add states on top of open/closed: a `state:<name>` label convention, a
//! Projects v2 board field, or open/closed plus assignee as a two-value stand-in. This adapter
//! uses **labels** — `state:in-progress`, `state:in-review`, and so on. Reasons:
//!
//! * It needs nothing beyond the REST Issues API this adapter already calls for
//!   `required_labels`. A Projects v2 field means a second API (GraphQL), a project number to
//!   configure, and a second pagination scheme.
//! * It supports as many states as the operator wants, unlike open/closed+assignee, which
//!   caps out at two.
//! * The label text *is* the state string — `active_states`/`terminal_states` in `symphony.toml`
//!   read the same vocabulary an operator already sees on the issue.
//!
//! `closed` always wins over any `state:*` label when deriving an issue's state — an operator
//! who closes an issue without remembering to swap its label must still see it as terminal, or
//! reconciliation could keep a workspace open under a ticket nobody is tracking anymore. An
//! issue with no `state:*` label reports as `"open"`, which is enough on its own for a repo
//! that has not adopted the label convention: `active_states = ["open"]` matches every open
//! issue with `required_labels`, same as this crate's own backlog today.
//!
//! ## Rate-limit budget
//!
//! One poll costs one paginated fetch for `by_states` (one request per 100 open, labelled
//! issues) plus one request per currently-running issue for `by_ids`. At the authenticated
//! primary limit of 5000 requests/hour, `interval_ms` should keep
//! `(1 + agent.max_concurrent) * (3_600_000 / interval_ms)` comfortably under that — a 30s
//! interval at `max_concurrent = 2` is `3 * 120 = 360/hour`, nowhere near the ceiling.
//! `symphony.github.toml` uses that combination. A 403/429 with a rate-limit signal still
//! classifies as [`crate::model::ErrorClass::RateLimited`] and backs off rather than escalates,
//! but a tight interval against a real repo will find that path often enough to be worth
//! avoiding up front.

use std::collections::HashMap;

use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{Tracker, TrackerError};
use crate::model::Issue;

const API_BASE: &str = "https://api.github.com";
const PER_PAGE: u32 = 100;
const API_VERSION: &str = "2022-11-28";

// ---- the HTTP seam ---------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    /// Lowercased header names, so lookups don't depend on what casing the caller used.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(String::as_str)
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("http transport error: {0}")]
pub struct HttpTransportError(pub String);

/// Everything `GithubTracker` needs from the network, and nothing more — one authenticated GET.
/// No POST/PATCH: this adapter is read-only by design (see [`super`]'s module doc), so nothing
/// on this trait can mutate a ticket.
pub trait Http: Send + Sync {
    fn get(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<HttpResponse, HttpTransportError>;
}

/// The real implementation, over `ureq`. Picked for its blocking API — `Tracker` methods are
/// synchronous, so an async client would need a runtime handle threaded through for no benefit
/// — and its default TLS backend is pure-Rust `rustls`, which needs no C toolchain to link.
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl Default for UreqHttp {
    fn default() -> Self {
        // ureq's default turns a non-2xx status into an `Err` that drops the response body and
        // headers — exactly the rate-limit header and body snippet `request()` needs to
        // classify the failure. Disabling it is what makes every status code, not just 2xx,
        // arrive as an ordinary `HttpResponse` for this adapter to read.
        let config = ureq::Agent::config_builder().http_status_as_error(false).build();
        Self { agent: ureq::Agent::new_with_config(config) }
    }
}

impl Http for UreqHttp {
    fn get(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<HttpResponse, HttpTransportError> {
        let mut req = self.agent.get(url);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        let mut resp = req.call().map_err(|e| HttpTransportError(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or_default().to_string()))
            .collect();
        let body = resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| HttpTransportError(format!("reading response body: {e}")))?;
        Ok(HttpResponse { status, headers, body })
    }
}

// ---- GitHub's REST shape, trimmed to what this adapter reads --------------

#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    node_id: String,
    title: String,
    state: String,
    html_url: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
    #[serde(default)]
    assignees: Vec<GhUser>,
    created_at: String,
    /// Present (any shape) only on pull requests — GitHub's Issues API returns PRs too. Its
    /// presence, not its content, is the signal.
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GhUser {
    #[allow(dead_code)]
    login: String,
}

/// `closed` overrides any `state:*` label; see the module doc for why. An issue with neither a
/// label nor a closed flag reports `"open"`.
fn derive_state(closed: bool, labels: &[String]) -> String {
    if closed {
        return "closed".into();
    }
    labels
        .iter()
        .find_map(|l| l.strip_prefix("state:"))
        .map(str::to_string)
        .unwrap_or_else(|| "open".into())
}

fn parse_created_at(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s, &Rfc3339).ok().map(|t| t.unix_timestamp() * 1000)
}

fn to_issue(owner: &str, repo: &str, gh: GhIssue) -> Issue {
    let labels: Vec<String> = gh.labels.into_iter().map(|l| l.name).collect();
    let closed = gh.state == "closed";
    Issue {
        id: format!("{owner}/{repo}#{}", gh.number),
        identifier: format!("#{}", gh.number),
        title: gh.title,
        state: derive_state(closed, &labels),
        priority: None, // GitHub has no native priority; an adapter over Projects v2 could add one.
        url: Some(gh.html_url),
        labels,
        // Assignment is the adapter-level eligibility signal here: an unassigned issue is not
        // yet ready for an agent to pick up, matching how this repo's own backlog is worked.
        dispatchable: !gh.assignees.is_empty(),
        created_at: parse_created_at(&gh.created_at),
        native_ref: Some(serde_json::json!({ "number": gh.number, "node_id": gh.node_id })),
        blocked_by: vec![], // GitHub's native issue-dependency graph is not read by this slice.
    }
}

// ---- the tracker ------------------------------------------------------------

pub struct GithubTracker<H: Http> {
    http: H,
    owner: String,
    repo: String,
    token: String,
    required_labels: Vec<String>,
}

impl<H: Http> GithubTracker<H> {
    pub fn new(http: H, owner: &str, repo: &str, token: &str, required_labels: &[String]) -> Self {
        Self {
            http,
            owner: owner.to_string(),
            repo: repo.to_string(),
            token: token.to_string(),
            required_labels: required_labels.to_vec(),
        }
    }

    fn headers(&self) -> Vec<(&str, String)> {
        vec![
            ("Authorization", format!("Bearer {}", self.token)),
            ("Accept", "application/vnd.github+json".to_string()),
            ("X-GitHub-Api-Version", API_VERSION.to_string()),
            ("User-Agent", "symphony-cc".to_string()),
        ]
    }

    /// One authenticated GET, classified onto [`TrackerError`]. The only branch a caller needs
    /// to handle specially beyond this is "404 on a single-issue fetch", which `by_ids` treats
    /// as absence rather than as an error.
    fn request(&self, url: &str) -> Result<HttpResponse, TrackerError> {
        let resp = self.http.get(url, &self.headers()).map_err(|e| TrackerError::Request(e.0))?;

        if (200..300).contains(&resp.status) {
            return Ok(resp);
        }
        if resp.status == 401 {
            return Err(TrackerError::Auth(body_snippet(&resp)));
        }
        let rate_limited = resp.status == 403 || resp.status == 429;
        let rate_limit_signal = resp.header("retry-after").is_some()
            || resp.header("x-ratelimit-remaining").is_some_and(|v| v == "0");
        if rate_limited && rate_limit_signal {
            return Err(TrackerError::RateLimited);
        }
        Err(TrackerError::Status(format!("{}: {}", resp.status, body_snippet(&resp))))
    }

    fn parse_issues(resp: &HttpResponse) -> Result<Vec<GhIssue>, TrackerError> {
        serde_json::from_slice(&resp.body).map_err(|e| TrackerError::Response(e.to_string()))
    }

    fn parse_issue(resp: &HttpResponse) -> Result<GhIssue, TrackerError> {
        serde_json::from_slice(&resp.body).map_err(|e| TrackerError::Response(e.to_string()))
    }

    /// Only issues with `required_labels` and `state=open`: a repo's closed-issue history can
    /// be arbitrarily large and is never active, so pulling it every poll would burn the rate
    /// budget on rows this call always discards. `by_ids` still sees closed issues — that is
    /// how reconciliation notices a running issue's ticket got closed.
    fn fetch_open_labelled(&self) -> Result<Vec<GhIssue>, TrackerError> {
        let labels = self.required_labels.join(",");
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let url = format!(
                "{API_BASE}/repos/{}/{}/issues?state=open&labels={}&per_page={PER_PAGE}&page={page}",
                self.owner,
                self.repo,
                urlencode(&labels),
            );
            // Fail the whole call on any page's error rather than returning what has been
            // gathered so far — a short list here is indistinguishable from "this is really
            // all of them," and the scheduler has no way to tell the difference back out.
            let resp = self.request(&url)?;
            let batch = Self::parse_issues(&resp)?;
            let got = batch.len();
            all.extend(batch);
            if got < PER_PAGE as usize {
                break;
            }
            page += 1;
        }
        Ok(all)
    }
}

impl<H: Http> Tracker for GithubTracker<H> {
    fn by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(vec![]);
        }
        let want: std::collections::HashSet<&str> = states.iter().map(|s| s.as_str()).collect();
        Ok(self
            .fetch_open_labelled()?
            .into_iter()
            .filter(|gh| gh.pull_request.is_none())
            .map(|gh| to_issue(&self.owner, &self.repo, gh))
            .filter(|issue| want.contains(issue.state_key().as_str()))
            .collect())
    }

    fn by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        for id in ids {
            // An id this tracker never issued (wrong owner/repo, or malformed) cannot be
            // "visible" to it — omit rather than error, same as a clean 404 below.
            let Some(number) = parse_dispatch_id(id, &self.owner, &self.repo) else { continue };

            let url = format!("{API_BASE}/repos/{}/{}/issues/{number}", self.owner, self.repo);
            let resp = match self.http.get(&url, &self.headers()) {
                Ok(r) => r,
                Err(e) => return Err(TrackerError::Request(e.0)),
            };
            if resp.status == 404 {
                continue; // genuinely gone: the caller's grace-count path handles this
            }
            if !(200..300).contains(&resp.status) {
                // Not "gone" — a real fetch failure. Surfacing it as an error rather than an
                // omission is the whole point of by_ids's "partial success is an error" rule:
                // the scheduler cannot otherwise tell a failed check apart from a clean miss.
                if resp.status == 401 {
                    return Err(TrackerError::Auth(body_snippet(&resp)));
                }
                let rate_limited = resp.status == 403 || resp.status == 429;
                let rate_limit_signal = resp.header("retry-after").is_some()
                    || resp.header("x-ratelimit-remaining").is_some_and(|v| v == "0");
                if rate_limited && rate_limit_signal {
                    return Err(TrackerError::RateLimited);
                }
                return Err(TrackerError::Status(format!(
                    "{}: {}",
                    resp.status,
                    body_snippet(&resp)
                )));
            }
            let gh = Self::parse_issue(&resp)?;
            if gh.pull_request.is_some() {
                continue; // a PR sharing the issue numbering space; never dispatchable
            }
            out.push(to_issue(&self.owner, &self.repo, gh));
        }
        Ok(out)
    }
}

fn parse_dispatch_id(id: &str, owner: &str, repo: &str) -> Option<u64> {
    let rest = id.strip_prefix(owner)?.strip_prefix('/')?.strip_prefix(repo)?.strip_prefix('#')?;
    rest.parse().ok()
}

fn body_snippet(resp: &HttpResponse) -> String {
    let text = String::from_utf8_lossy(&resp.body);
    text.chars().take(200).collect()
}

fn urlencode(s: &str) -> String {
    // The only characters this adapter ever puts in a query value are label names and commas;
    // this covers exactly those without pulling in a general-purpose URL crate.
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ',' => "%2C".to_string(),
            ' ' => "%20".to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    struct FakeHttp {
        inner: Mutex<FakeHttpInner>,
    }

    #[derive(Default)]
    struct FakeHttpInner {
        responses: VecDeque<Result<HttpResponse, HttpTransportError>>,
        calls: Vec<String>,
    }

    impl FakeHttp {
        fn new() -> Self {
            Self { inner: Mutex::new(FakeHttpInner::default()) }
        }

        fn push(&self, resp: Result<HttpResponse, HttpTransportError>) {
            self.inner.lock().unwrap().responses.push_back(resp);
        }

        fn calls(&self) -> Vec<String> {
            self.inner.lock().unwrap().calls.clone()
        }
    }

    impl Http for FakeHttp {
        fn get(
            &self,
            url: &str,
            _headers: &[(&str, String)],
        ) -> Result<HttpResponse, HttpTransportError> {
            let mut g = self.inner.lock().unwrap();
            g.calls.push(url.to_string());
            g.responses
                .pop_front()
                .unwrap_or_else(|| Err(HttpTransportError("no scripted response".into())))
        }
    }

    fn ok(body: serde_json::Value) -> Result<HttpResponse, HttpTransportError> {
        Ok(HttpResponse {
            status: 200,
            headers: HashMap::new(),
            body: body.to_string().into_bytes(),
        })
    }

    fn status(code: u16, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpTransportError> {
        Ok(HttpResponse {
            status: code,
            headers: headers.iter().map(|(k, v)| (k.to_lowercase(), v.to_string())).collect(),
            body: b"{}".to_vec(),
        })
    }

    fn gh_issue(number: u64, labels: &[&str], state: &str, assigned: bool) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "node_id": format!("node-{number}"),
            "title": format!("issue {number}"),
            "state": state,
            "html_url": format!("https://github.com/o/r/issues/{number}"),
            "labels": labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>(),
            "assignees": if assigned { vec![serde_json::json!({"login": "someone"})] } else { vec![] },
            "created_at": "2024-01-15T10:30:00Z",
        })
    }

    fn tracker(http: FakeHttp) -> GithubTracker<FakeHttp> {
        GithubTracker::new(http, "o", "r", "tok", &["agent".to_string()])
    }

    #[test]
    fn empty_queries_make_no_request() {
        let http = FakeHttp::new();
        let t = tracker(http);
        assert!(t.by_states(&[]).unwrap().is_empty());
        assert!(t.by_ids(&[]).unwrap().is_empty());
        assert!(t.http.calls().is_empty());
    }

    #[test]
    fn a_full_page_triggers_a_second_request_a_short_page_does_not() {
        let http = FakeHttp::new();
        let full_page: Vec<_> =
            (1..=PER_PAGE as u64).map(|n| gh_issue(n, &["state:open"], "open", true)).collect();
        http.push(ok(serde_json::Value::Array(full_page)));
        http.push(ok(serde_json::json!([gh_issue(
            PER_PAGE as u64 + 1,
            &["state:open"],
            "open",
            true
        )])));

        let t = tracker(http);
        let got = t.by_states(&["open".to_string()]).unwrap();

        assert_eq!(got.len(), PER_PAGE as usize + 1);
        assert_eq!(t.http.calls().len(), 2, "a full page must be followed by a page-2 request");
        assert!(t.http.calls()[0].contains("page=1"));
        assert!(t.http.calls()[1].contains("page=2"));
    }

    #[test]
    fn a_failure_on_the_second_page_fails_the_whole_call_not_a_short_list() {
        let http = FakeHttp::new();
        let full_page: Vec<_> =
            (1..=PER_PAGE as u64).map(|n| gh_issue(n, &["state:open"], "open", true)).collect();
        http.push(ok(serde_json::Value::Array(full_page)));
        http.push(status(500, &[]));

        let t = tracker(http);
        let err = t.by_states(&["open".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Status(_)));
    }

    #[test]
    fn rate_limit_signals_classify_distinctly_from_a_bare_403() {
        let http = FakeHttp::new();
        http.push(status(403, &[("retry-after", "60")]));
        let err = tracker(http).by_states(&["open".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::RateLimited));

        let http = FakeHttp::new();
        http.push(status(403, &[]));
        let err = tracker(http).by_states(&["open".to_string()]).unwrap_err();
        assert!(
            matches!(err, TrackerError::Status(_)),
            "a bare 403 is a permission error, not a rate limit"
        );
    }

    #[test]
    fn an_expired_token_classifies_as_auth_not_status() {
        let http = FakeHttp::new();
        http.push(status(401, &[]));
        let err = tracker(http).by_states(&["open".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Auth(_)));
    }

    #[test]
    fn pull_requests_are_excluded_from_by_states() {
        let http = FakeHttp::new();
        let mut pr = gh_issue(2, &["state:open"], "open", true);
        pr["pull_request"] = serde_json::json!({});
        http.push(ok(serde_json::json!([gh_issue(1, &["state:open"], "open", true), pr])));

        let got = tracker(http).by_states(&["open".to_string()]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].identifier, "#1");
    }

    #[test]
    fn closed_overrides_a_stale_state_label() {
        let http = FakeHttp::new();
        http.push(ok(serde_json::json!(gh_issue(5, &["state:in-progress"], "closed", true))));
        let got = tracker(http).by_ids(&["o/r#5".to_string()]).unwrap();
        assert_eq!(got[0].state, "closed");
    }

    #[test]
    fn a_404_on_by_ids_is_omitted_not_an_error() {
        let http = FakeHttp::new();
        http.push(status(404, &[]));
        let got = tracker(http).by_ids(&["o/r#9".to_string()]).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn a_non_404_failure_on_by_ids_fails_the_whole_call() {
        let http = FakeHttp::new();
        http.push(status(500, &[]));
        let err = tracker(http).by_ids(&["o/r#9".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Status(_)));
    }

    #[test]
    fn an_id_from_a_different_repo_is_omitted_without_a_request() {
        let http = FakeHttp::new();
        let got = tracker(http).by_ids(&["other/repo#1".to_string()]).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn dispatchability_follows_assignment() {
        let http = FakeHttp::new();
        http.push(ok(serde_json::json!([gh_issue(1, &["state:open"], "open", false)])));
        let got = tracker(http).by_states(&["open".to_string()]).unwrap();
        assert!(!got[0].dispatchable, "an unassigned issue is not yet ready for an agent");
    }

    #[test]
    fn the_dispatch_id_is_stable_and_never_the_title() {
        let http = FakeHttp::new();
        http.push(ok(serde_json::json!([gh_issue(42, &["state:open"], "open", true)])));
        let got = tracker(http).by_states(&["open".to_string()]).unwrap();
        assert_eq!(got[0].id, "o/r#42");
    }

    #[test]
    fn derive_state_prefers_closed_then_label_then_open() {
        assert_eq!(derive_state(true, &["state:in-progress".to_string()]), "closed");
        assert_eq!(derive_state(false, &["state:in-progress".to_string()]), "in-progress");
        assert_eq!(derive_state(false, &[]), "open");
    }
}

/// `FakeHttp` above proves `GithubTracker`'s classification logic is correct given a properly
/// shaped `HttpResponse` — it says nothing about whether `UreqHttp` actually produces one.
/// ureq's default (`http_status_as_error: true`) turns a non-2xx response into an `Err` that
/// discards the body and headers this adapter classifies on, which every `FakeHttp` test is
/// structurally blind to: it compiled, and every other test stayed green, while a live 404
/// came back as `TrackerError::Request` instead of an empty result. These tests talk to a raw
/// `TcpListener` rather than a real server so they still run with no network.
#[cfg(test)]
mod ureq_http_tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// Accepts exactly one connection, writes a fixed raw HTTP/1.1 response, and hands back the
    /// port it bound so a test can point `UreqHttp` at it.
    fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf); // drain the request so the client isn't left hanging
            stream.write_all(response.as_bytes()).unwrap();
        });
        port
    }

    #[test]
    fn a_404_arrives_as_a_response_not_an_error() {
        let port = serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}");
        let http = UreqHttp::default();
        let resp = http.get(&format!("http://127.0.0.1:{port}/"), &[]).unwrap();
        assert_eq!(resp.status, 404, "a 404 must reach the caller as a response, not Err(_)");
    }

    #[test]
    fn rate_limit_headers_survive_a_non_2xx_response() {
        let port =
            serve_once("HTTP/1.1 403 Forbidden\r\nRetry-After: 30\r\nContent-Length: 2\r\n\r\n{}");
        let http = UreqHttp::default();
        let resp = http.get(&format!("http://127.0.0.1:{port}/"), &[]).unwrap();
        assert_eq!(resp.status, 403);
        assert_eq!(resp.header("retry-after"), Some("30"));
    }

    #[test]
    fn a_2xx_body_still_parses_normally() {
        let port = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"ok\": true}\n");
        let http = UreqHttp::default();
        let resp = http.get(&format!("http://127.0.0.1:{port}/"), &[]).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"{\"ok\": true}\n");
    }
}
