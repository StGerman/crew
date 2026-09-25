//! The loopback listener that receives GitHub's redirect, and stops.
//!
//! It accepts a code that converts into the App's private key, so three things are held here
//! rather than hoped for. It binds `127.0.0.1` and nothing else. It answers only requests whose
//! `Host` is its own address, so a page that points its own name at `127.0.0.1` cannot read the
//! init page — and the nonce in it — as same-origin. And it stops accepting as soon as one
//! callback carrying a code has arrived, whatever its `state`: [`await_callback`] joins the
//! accept thread before returning, so "the listener is gone" is true by the time the caller
//! learns anything.
//!
//! A browser is not a well-behaved MCP client — it preconnects sockets it never writes to and
//! holds keep-alive connections open — so each connection is read on its own short-lived thread
//! with [`Limits`]' deadlines, and one request is served per connection (`Connection: close`).
//! A socket that never speaks costs one thread for one deadline, never the callback.

use std::io::{BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crate::broker::server::{ConnSlot, Limits, Request, read_request};

use super::InitError;

pub struct Listener {
    listener: TcpListener,
    addr: SocketAddr,
}

impl Listener {
    pub fn bind() -> Result<Self, InitError> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .map_err(|e| InitError::Listen(e.to_string()))?;
        let addr = listener.local_addr().map_err(|e| InitError::Listen(e.to_string()))?;
        Ok(Self { listener, addr })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// The accepted callback, with the connection the browser is waiting on. The caller converts the
/// code first and answers second, so the answer can send the browser straight on to the install
/// page — the operator's second and last click.
pub struct Callback {
    pub code: String,
    stream: TcpStream,
}

impl Callback {
    pub fn redirect(mut self, location: &str, html: &str) {
        // Best-effort: the browser may have given up; the App exists either way and the caller
        // also prints the install URL.
        let _ = write_response(&mut self.stream, "302 Found", Some(location), html);
    }

    pub fn fail(mut self, html: &str) {
        // Best-effort, as above: the error the caller returns is the report that matters.
        let _ = write_response(&mut self.stream, "400 Bad Request", None, html);
    }
}

/// Serve `page` at `/` until a request reaches `/callback`, then stop accepting.
///
/// A callback whose `state` is not `nonce` ends the run with [`InitError::StateMismatch`] rather
/// than waiting for a better one: something other than this run's page reached the callback, and
/// the operator should know that before a key is minted anywhere.
pub fn await_callback(
    listener: Listener,
    nonce: &str,
    page: &str,
    limits: Limits,
) -> Result<Callback, InitError> {
    let Listener { listener, addr } = listener;
    let hosts = [format!("127.0.0.1:{}", addr.port()), format!("localhost:{}", addr.port())];
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<(Request, TcpStream)>();
    let accept = spawn_acceptor(listener, Arc::clone(&stop), tx, limits)?;

    let outcome = loop {
        let Ok((req, mut stream)) = rx.recv() else {
            break Err(InitError::Listen("the callback listener stopped".into()));
        };
        if !req.host.as_deref().is_some_and(|h| hosts.iter().any(|a| a == h)) {
            tracing::warn!(host = ?req.host, "refused a request for another host");
            let _ = write_response(&mut stream, "421 Misdirected Request", None, "");
            continue;
        }
        let (route, query) = req.path.split_once('?').unwrap_or((&req.path, ""));
        match (req.method.as_str(), route) {
            ("GET", "/") => {
                // Best-effort: a browser that went away can reload the printed URL.
                let _ = write_response(&mut stream, "200 OK", None, page);
            }
            ("GET", "/callback") => break check(query, nonce, stream),
            _ => {
                let _ = write_response(&mut stream, "404 Not Found", None, "");
            }
        }
    };

    stop.store(true, Ordering::Release);
    // The accept thread only looks at `stop` after an accept returns, so give it one.
    drop(TcpStream::connect_timeout(&addr, Duration::from_secs(1)));
    // The listener is dropped as that thread ends; joining is what makes it gone *now*.
    let _ = accept.join();
    outcome
}

fn check(query: &str, nonce: &str, mut stream: TcpStream) -> Result<Callback, InitError> {
    let param = |name: &str| {
        query.split('&').find_map(|kv| kv.strip_prefix(name)?.strip_prefix('=')).map(decode)
    };
    if param("state").as_deref() != Some(nonce) {
        let _ = write_response(
            &mut stream,
            "400 Bad Request",
            None,
            "This callback was not started by this run of crewd init. Nothing was created.",
        );
        return Err(InitError::StateMismatch);
    }
    match param("code").filter(|c| !c.is_empty()) {
        Some(code) => Ok(Callback { code, stream }),
        None => {
            let _ = write_response(&mut stream, "400 Bad Request", None, "No code in callback.");
            Err(InitError::NoCode)
        }
    }
}

fn spawn_acceptor(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<(Request, TcpStream)>,
    limits: Limits,
) -> Result<std::thread::JoinHandle<()>, InitError> {
    // Logs from these threads reach whatever the caller logs to, so a test capturing output sees
    // the whole run and not only the half on its own thread.
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let live = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("crewd-init-accept".into())
        .spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    // Checked before the thread exists, as the MCP transport does: a local
                    // process opening sockets it never writes to would otherwise cost a thread
                    // each for a full deadline, without bound.
                    let Some(slot) = ConnSlot::take(&live, limits.max_connections) else {
                        tracing::warn!(
                            max = limits.max_connections,
                            "refusing a connection: too many already in flight"
                        );
                        continue;
                    };
                    let tx = tx.clone();
                    let dispatch = dispatch.clone();
                    let spawned = std::thread::Builder::new().name("crewd-init-conn".into()).spawn(
                        move || {
                            let _slot = slot;
                            tracing::dispatcher::with_default(&dispatch, || {
                                read_one(stream, tx, limits)
                            })
                        },
                    );
                    if let Err(e) = spawned {
                        tracing::warn!(error = %e, "could not serve a connection");
                    }
                }
            })
        })
        .map_err(|e| InitError::Listen(e.to_string()))
}

fn read_one(stream: TcpStream, tx: mpsc::Sender<(Request, TcpStream)>, limits: Limits) {
    let Ok(read_half) = stream.try_clone() else { return };
    let _ = stream.set_write_timeout(Some(limits.request));
    // A preconnected socket that never speaks is ordinary for a browser; the idle deadline is
    // what ends it.
    let limits = Limits { idle: limits.request, ..limits };
    match read_request(&mut BufReader::new(read_half), limits) {
        Ok(Some(req)) => {
            // The receiver is gone once the callback has been handled; this request is then
            // simply dropped with its connection.
            let _ = tx.send((req, stream));
        }
        Ok(None) => {}
        Err(e) => tracing::debug!(error = %e, "init connection ended"),
    }
}

fn write_response(
    w: &mut TcpStream,
    status: &str,
    location: Option<&str>,
    html: &str,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n",
        html.len()
    );
    if let Some(location) = location {
        head.push_str(&format!("Location: {location}\r\n"));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(html.as_bytes())?;
    w.flush()
}

/// Percent-decoding for the two query values GitHub sends. The code is URL-safe already; this
/// only has to be right for what a hostile caller might send, which is rejected either way.
fn decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let pair =
            bytes.get(i + 1).copied().and_then(hex).zip(bytes.get(i + 2).copied().and_then(hex));
        match (bytes[i], pair) {
            (b'%', Some((hi, lo))) => {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
            (b'+', _) => out.push(b' '),
            (b, _) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn connections_past_the_cap_are_refused_rather_than_each_given_a_thread() {
        let listener = Listener::bind().unwrap();
        let addr = listener.addr();
        let limits = Limits {
            idle: Duration::from_secs(2),
            request: Duration::from_secs(2),
            max_connections: 1,
        };
        let server = std::thread::spawn(move || {
            await_callback(listener, "nonce", "page", limits).map(|_| ()).unwrap_err()
        });

        // Holds the only slot and says nothing, as a preconnecting browser does.
        let _idle = TcpStream::connect(addr).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let mut refused = TcpStream::connect(addr).unwrap();
        refused.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut buf = [0u8; 1];
        // Closed at once, not held open until a read deadline: a timeout here means a thread
        // is sitting on this socket.
        let got = refused.read(&mut buf);
        assert!(
            matches!(&got, Ok(0))
                || matches!(&got, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
            "a connection past the cap was served: {got:?}"
        );

        drop(_idle);
        // The idle socket's deadline frees its slot; the next request is served.
        let mut s = loop {
            std::thread::sleep(Duration::from_millis(200));
            let mut s = TcpStream::connect(addr).unwrap();
            let req = format!("GET /callback?code=c&state=wrong HTTP/1.1\r\nHost: {addr}\r\n\r\n");
            if s.write_all(req.as_bytes()).is_ok() {
                s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut peek = [0u8; 1];
                if matches!(s.peek(&mut peek), Ok(n) if n > 0) {
                    break s;
                }
            }
        };
        let mut raw = String::new();
        let _ = s.read_to_string(&mut raw);
        assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
        assert!(matches!(server.join().unwrap(), InitError::StateMismatch));
    }
}
