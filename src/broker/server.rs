//! MCP over HTTP, on loopback, hand-rolled.
//!
//! ## Why not `rmcp`
//!
//! The issue proposed the official Rust SDK. It was tried and set aside for two concrete
//! reasons, neither of them stylistic:
//!
//! * `rmcp`'s `transport-streamable-http-server` is a `tower::Service`, not a server. It ships
//!   no listener, so it does not remove the need for an HTTP stack — it adds ~35 crates *and*
//!   still requires axum or hyper on top of them. The dependency footprint for three tools
//!   would roughly double this crate's tree.
//! * It is async, and everything it would sit between is not. [`Tracker`] and
//!   [`TrackerWrites`] are blocking by deliberate choice (see [`crate::tracker::github`]'s note
//!   on `ureq`), and `Scheduler::tick` is a synchronous function. Bridging them would mean a
//!   runtime handle threaded through the broker and `spawn_blocking` around every write, for a
//!   protocol surface of four methods.
//!
//! Four methods is what this actually is: `initialize`, `notifications/initialized`,
//! `tools/list`, `tools/call`. The rest of MCP's streamable-HTTP transport is optional for a
//! server that never initiates a message.
//!
//! [`Tracker`]: crate::tracker::Tracker
//! [`TrackerWrites`]: super::TrackerWrites
//!
//! ## What the real client does
//!
//! Confirmed by pointing `claude 2.1.278` at a recording server rather than read off a spec,
//! because the parts that would have been guessed wrong are exactly the parts that are not in
//! the spec's happy path:
//!
//! * The first request is **`server/discover`**, not `initialize`, carrying
//!   `mcp-protocol-version: 2026-07-28`. It is not in the revision this server implements, and
//!   answering it with a plain JSON-RPC `-32601` is accepted — the client falls through to the
//!   normal handshake. Nothing here needs to know what it is.
//! * `initialize` then negotiates `2025-11-25`. The server echoes whatever version it is
//!   offered rather than asserting one.
//! * `notifications/initialized` arrives with **no `id`**. A JSON-RPC notification takes no
//!   response body at all: `202 Accepted`, empty. Answering it like a request breaks the
//!   handshake.
//! * The client opens a **`GET` for an SSE stream**. Refusing it with `405` is fine and costs
//!   nothing — this server has no server-initiated messages to push, so a plain
//!   `application/json` response to each POST is the entire transport. That single fact is
//!   what makes this file short enough to be worth owning.
//! * Requests are keep-alive on one connection, so the read loop must serve many requests per
//!   accept rather than closing after the first.
//!
//! ## Exposure
//!
//! Bound to `127.0.0.1` on an ephemeral port — never a routable interface. Authority is the
//! per-run token in the path, checked in [`Broker::call`](super::Broker::call). A tool failure
//! is reported as an MCP tool *result* with `isError: true`, never as a JSON-RPC error: the
//! former is something the agent can read and work around, the latter reads as a broken
//! server and is the shape that would turn a refused write into a failed run.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;

use serde_json::{Value, json};

use super::Broker;

/// The revision this server implements when the client does not name one.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-25";

/// Caps on one request, so a hostile or wedged client cannot turn a connection into a memory
/// problem. A comment body is bounded well below this by the broker's own argument limit.
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Bind the broker's listener on loopback, letting the OS pick the port.
///
/// Separate from [`serve`] so the caller can learn the address *before* constructing the
/// [`Broker`] that has to embed it in every session URL.
pub fn bind() -> std::io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
}

/// Start serving on a background thread. Returns immediately.
///
/// There is no shutdown handle: the listener lives as long as the process, and every session
/// it could serve is revoked independently when its [`BrokerSession`](super::BrokerSession) is
/// dropped. Stopping the listener would add a second thing to get right for no property the
/// token lifetime does not already provide.
pub fn serve(broker: Arc<Broker>, listener: TcpListener) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let broker = Arc::clone(&broker);
                    std::thread::spawn(move || {
                        if let Err(e) = handle_conn(&broker, s) {
                            tracing::debug!(error = %e, "broker connection ended");
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %e, "broker accept failed"),
            }
        }
    });
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn handle_conn(broker: &Broker, stream: TcpStream) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    // Keep-alive: one accept serves the whole run's traffic.
    while let Some(req) = read_request(&mut reader)? {
        let token = req.path.strip_prefix("/mcp/").unwrap_or("").to_string();

        match req.method.as_str() {
            // The SSE stream. Nothing here ever pushes, so decline it.
            "GET" => respond(&mut writer, 405, "Method Not Allowed", None)?,
            // The client's session teardown.
            "DELETE" => respond(&mut writer, 200, "OK", None)?,
            "POST" => {
                let parsed: Result<Value, _> = serde_json::from_slice(&req.body);
                let Ok(rpc) = parsed else {
                    let body = serde_json::to_vec(&error_body(Value::Null, -32700, "parse error"))
                        .unwrap_or_default();
                    respond(&mut writer, 200, "OK", Some(&body))?;
                    continue;
                };
                match handle_rpc(broker, &token, &rpc) {
                    // A notification: acknowledged, never answered.
                    None => respond(&mut writer, 202, "Accepted", None)?,
                    Some(v) => {
                        let body = serde_json::to_vec(&v).unwrap_or_default();
                        respond(&mut writer, 200, "OK", Some(&body))?;
                    }
                }
            }
            _ => respond(&mut writer, 405, "Method Not Allowed", None)?,
        }
    }
    Ok(())
}

/// `None` means the message was a notification and takes no reply.
fn handle_rpc(broker: &Broker, token: &str, rpc: &Value) -> Option<Value> {
    let method = rpc.get("method").and_then(Value::as_str).unwrap_or_default();
    let id = rpc.get("id").cloned();

    // No id at all: a notification. `notifications/initialized` is the one that matters, but
    // the rule is general — anything without an id gets no response body.
    let id = id?;

    let result = match method {
        "initialize" => {
            let version = rpc
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": super::SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": broker.tools_json() }),
        "tools/call" => {
            let name = rpc.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
            let empty = json!({});
            let args = rpc.pointer("/params/arguments").unwrap_or(&empty);

            // A refusal is a tool *result*, not a transport error: the agent is meant to read
            // it and pick something else, which is the "a tool failure is not a run failure"
            // rule from the issue.
            match broker.call(token, name, args) {
                Ok(text) => json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false
                }),
                Err(e) => json!({
                    "content": [{ "type": "text", "text": e.to_string() }],
                    "isError": true
                }),
            }
        }
        // Everything else, `server/discover` included. See the module doc: the real client
        // handles this and continues.
        other => {
            return Some(error_body(id, -32601, &format!("method not found: {other}")));
        }
    };

    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn error_body(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// `Ok(None)` on a clean EOF — the client closed, which is how every run ends.
fn read_request(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let mut start = String::new();
    if reader.read_line(&mut start)? == 0 {
        return Ok(None);
    }
    // Tolerate a stray blank line between keep-alive requests.
    while start.trim().is_empty() {
        start.clear();
        if reader.read_line(&mut start)? == 0 {
            return Ok(None);
        }
    }

    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    let mut header_bytes = start.len();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err(std::io::Error::other("request headers too large"));
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    if content_length > MAX_BODY_BYTES {
        return Err(std::io::Error::other("request body too large"));
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Request { method, path, body }))
}

fn respond(
    w: &mut TcpStream,
    status: u16,
    reason: &str,
    body: Option<&[u8]>,
) -> std::io::Result<()> {
    let body = body.unwrap_or(b"");
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n",
        body.len()
    );
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// Transport tests over a real socket.
///
/// Deliberately not driven through a client library: the bugs this file can have are framing
/// bugs — a missing `Content-Length`, a notification answered with a body, a keep-alive
/// connection closed after one request — and every one of them is invisible to a test that
/// hands the server a pre-parsed request. Same reasoning as `ureq_http_tests` in
/// [`crate::tracker::github`]. The request sequence below is the one `claude 2.1.278` actually
/// sends, recorded from a live handshake.
#[cfg(test)]
mod tests {
    use std::io::BufRead;
    use std::sync::Arc;

    use super::*;
    use crate::broker::fake::FakeWrites;
    use crate::broker::{BrokerLimits, BrokerSession, TrackerWrites};
    use crate::clock::FakeClock;
    use crate::model::Issue;

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

    /// A live broker, serving on a real loopback port, with one open session.
    fn serving(tag: &str) -> (Arc<Broker>, Arc<FakeWrites>, BrokerSession, SocketAddr) {
        let writes = Arc::new(FakeWrites::new());
        let w: Arc<dyn TrackerWrites> = writes.clone();
        let dir = std::env::temp_dir().join(format!(
            "symphony-broker-srv-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let listener = bind().unwrap();
        let addr = listener.local_addr().unwrap();
        let broker = Arc::new(
            Broker::new(
                w,
                Arc::new(FakeClock::new()),
                BrokerLimits::default(),
                vec!["in progress".into(), "done".into()],
                addr,
                dir,
            )
            .unwrap(),
        );
        serve(Arc::clone(&broker), listener);
        let session = broker.open(&issue("o/r#1"), "run-1").unwrap();
        (broker, writes, session, addr)
    }

    struct Conn {
        stream: TcpStream,
        reader: BufReader<TcpStream>,
    }

    impl Conn {
        fn open(addr: SocketAddr) -> Self {
            let stream = TcpStream::connect(addr).unwrap();
            let reader = BufReader::new(stream.try_clone().unwrap());
            Self { stream, reader }
        }

        /// Returns the status code and body. Panics rather than erroring: a framing failure
        /// here is the bug under test, and a panic names the line.
        fn send(&mut self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
            let body = body.unwrap_or("");
            let req = format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: \
                 application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            self.stream.write_all(req.as_bytes()).unwrap();
            self.stream.flush().unwrap();

            let mut status_line = String::new();
            self.reader.read_line(&mut status_line).unwrap();
            let status: u16 = status_line.split_whitespace().nth(1).unwrap_or("0").parse().unwrap();

            let mut len = 0usize;
            loop {
                let mut line = String::new();
                self.reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = line.split_once(':')
                    && k.trim().eq_ignore_ascii_case("content-length")
                {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut buf = vec![0u8; len];
            if len > 0 {
                self.reader.read_exact(&mut buf).unwrap();
            }
            (status, String::from_utf8_lossy(&buf).to_string())
        }

        fn rpc(&mut self, path: &str, body: &str) -> Value {
            let (status, text) = self.send("POST", path, Some(body));
            assert_eq!(status, 200, "body was {text}");
            serde_json::from_str(&text).expect("a JSON-RPC response")
        }
    }

    #[test]
    fn the_handshake_the_real_client_performs_is_answered_end_to_end() {
        let (_b, writes, session, addr) = serving("handshake");
        let path = format!("/mcp/{}", session.token);
        let mut c = Conn::open(addr);

        // 1. `server/discover`, which this revision does not implement. A plain -32601 is
        //    what the real client falls through on.
        let discover =
            c.rpc(&path, r#"{"jsonrpc":"2.0","id":"d1","method":"server/discover","params":{}}"#);
        assert_eq!(discover["error"]["code"], -32601);

        // 2. initialize, echoing the version offered rather than asserting one.
        let init = c.rpc(
            &path,
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#,
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
        assert!(init["result"]["capabilities"]["tools"].is_object());

        // 3. The initialized notification: no id, so no response body at all.
        let (status, body) = c.send(
            "POST",
            &path,
            Some(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        );
        assert_eq!(status, 202, "a notification takes no reply");
        assert!(body.is_empty(), "answering a notification breaks the handshake, got {body}");

        // 4. The SSE stream this server does not offer.
        let (status, _) = c.send("GET", &path, None);
        assert_eq!(status, 405);

        // 5. tools/list, on the same keep-alive connection as everything above.
        let list = c.rpc(&path, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);

        // 6. A call that does real work.
        let call = c.rpc(
            &path,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"comment","arguments":{"body":"from the wire"},"_meta":{"progressToken":2}}}"#,
        );
        assert_eq!(call["result"]["isError"], false);
        assert_eq!(writes.count(), 1, "exactly one tracker write reached the orchestrator");
    }

    #[test]
    fn a_refused_call_comes_back_as_a_tool_error_not_a_transport_error() {
        // The shape matters: a JSON-RPC error reads to the client as a broken server, which is
        // how a refused write turns into a failed run. `isError` on the result is something
        // the agent can read and route around.
        let (b, writes, session, addr) = serving("refusal");
        let mut c = Conn::open(addr);

        let call = c.rpc(
            &format!("/mcp/{}", session.token),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"comment","arguments":{"body":"hi","issue_id":"o/r#99"}}}"#,
        );
        assert!(call.get("error").is_none(), "must not be a JSON-RPC error: {call}");
        assert_eq!(call["result"]["isError"], true);
        assert!(
            call["result"]["content"][0]["text"].as_str().unwrap().contains("issue_id"),
            "the agent is told what was wrong: {call}"
        );
        assert_eq!(writes.count(), 0);
        assert_eq!(b.audit().len(), 1);
    }

    #[test]
    fn a_forged_token_reaches_no_issue() {
        let (_b, writes, _session, addr) = serving("forged");
        let mut c = Conn::open(addr);

        let call = c.rpc(
            "/mcp/0000000000000000000000000000000000000000000000000000000000000000",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"comment","arguments":{"body":"hi"}}}"#,
        );
        assert_eq!(call["result"]["isError"], true);
        assert_eq!(writes.count(), 0);
    }

    #[test]
    fn a_malformed_body_does_not_take_the_connection_down_with_it() {
        let (_b, _w, session, addr) = serving("garbage");
        let path = format!("/mcp/{}", session.token);
        let mut c = Conn::open(addr);

        let (status, text) = c.send("POST", &path, Some("{not json"));
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["error"]["code"], -32700);

        // The connection must still be usable: one bad frame from a confused client should not
        // cost the run every tool call after it.
        let list = c.rpc(&path, r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#);
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 3);
    }
}
