//! The dev proxy: listens on the app's address, forwards every request to
//! the app on its internal port, adds the live reload client to HTML pages
//! and pushes events to open browsers over server-sent events.
//!
//! It is a plain HTTP/1.1 proxy on threads (one per connection): fast
//! enough for one developer, with no async runtime in the CLI. Request and
//! response bodies stream through untouched except HTML pages, which get
//! one `<script>` tag: a page that arrives in one go is buffered and sent
//! with its new length; one the app streams (it pauses, or passes
//! [`MAX_INJECT`]) is forwarded as it arrives, re-chunked, with the tag
//! spliced in on the way. WebSocket (and other) upgrades are tunnelled, so
//! live views and websockets keep working.
//!
//! Each forwarded request opens its own connection to the app with
//! `Connection: close`, so switching to a rebuilt app ([`Hub::set_upstream`])
//! takes effect on the very next request while the old app finishes the
//! ones in flight.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use super::inject;
use crate::build::Diagnostic;

/// Largest request or response head accepted.
const MAX_HEAD: usize = 64 * 1024;
/// Most of an HTML page buffered to add the client. A larger page is
/// streamed to the browser instead (the client still added on the way).
const MAX_INJECT: usize = 32 * 1024 * 1024;
/// How long an HTML page may pause before the proxy stops waiting for the
/// rest and streams what it has (a page the app writes progressively).
const HTML_PAUSE: Duration = Duration::from_millis(100);
/// Most bytes of an HTML body read at a time.
const PIECE: u64 = 64 * 1024;
/// How long an idle keep-alive connection from a browser stays open.
const IDLE: Duration = Duration::from_secs(120);
/// How often open event streams get a keep-alive comment.
const PING: Duration = Duration::from_secs(15);
/// How long a write to a browser's event stream may block.
const EVENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

// ---- events and the hub ----------------------------------------------------------------

/// What went wrong, shown as an overlay in every open page until cleared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The overlay's heading, e.g. "Build failed".
    pub title: String,
    /// One line under the heading.
    pub note: String,
    /// Compiler errors (may be empty).
    pub errors: Vec<Diagnostic>,
}

impl Problem {
    /// The `problem` event's JSON payload: rustc's colour codes stripped.
    ///
    /// ```
    /// use zapline_cli::dev::proxy::Problem;
    /// let p = Problem { title: "Build failed".into(), note: String::new(), errors: vec![] };
    /// assert_eq!(p.to_json()["title"], "Build failed");
    /// ```
    pub fn to_json(&self) -> serde_json::Value {
        let errors: Vec<_> = self
            .errors
            .iter()
            .map(|d| {
                json!({
                    "file": d.file,
                    "line": d.line,
                    "column": d.column,
                    "message": d.message,
                    "rendered": inject::strip_ansi(&d.rendered),
                })
            })
            .collect();
        json!({ "title": self.title, "note": self.note, "errors": errors })
    }
}

/// An event pushed to open pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Reload the page.
    Reload,
    /// Re-fetch these stylesheets (paths relative to the project root)
    /// without reloading.
    Css(Vec<String>),
    /// A rebuild started: show the badge.
    Building,
    /// The rebuild finished (successfully or not): hide the badge.
    Ready,
    /// Show the overlay.
    Problem(Problem),
    /// Hide the overlay.
    Clear,
}

impl Event {
    /// The event as one server-sent-events frame.
    ///
    /// ```
    /// use zapline_cli::dev::proxy::Event;
    /// assert_eq!(Event::Reload.frame(), "event: reload\ndata: {}\n\n");
    /// ```
    pub fn frame(&self) -> String {
        let (name, data) = match self {
            Event::Reload => ("reload", json!({})),
            Event::Css(paths) => ("css", json!({ "paths": paths })),
            Event::Building => ("building", json!({})),
            Event::Ready => ("ready", json!({})),
            Event::Problem(p) => ("problem", p.to_json()),
            Event::Clear => ("clear", json!({})),
        };
        frame(name, &data)
    }
}

fn frame(name: &str, data: &serde_json::Value) -> String {
    // serde_json never emits raw newlines, so one `data:` line suffices.
    format!("event: {name}\ndata: {data}\n\n")
}

/// The header the proxy sends [`Hub::key`] in (any copy a client sends is
/// dropped).
pub const KEY_HEADER: &str = "x-zapline-dev-proxy";

/// 128 random bits as hex, from the standard library's per-process random
/// hash keys.
fn random_key() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(32);
    for i in 0u8..2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u8(i);
        h.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Shared state of the proxy: where the app is, what pages should show,
/// and the open event streams.
#[derive(Debug)]
pub struct Hub {
    epoch: String,
    key: String,
    state: Mutex<HubState>,
}

#[derive(Debug, Default)]
struct HubState {
    upstream: Option<SocketAddr>,
    building: bool,
    problem: Option<Problem>,
    /// Why there is no upstream, for the placeholder page.
    down: Option<String>,
    clients: Vec<TcpStream>,
}

impl Hub {
    /// A hub with no app yet. Starts a thread that keeps event streams
    /// alive for as long as the hub exists.
    ///
    /// ```
    /// let hub = zapline_cli::dev::proxy::Hub::new();
    /// assert_eq!(hub.upstream(), None);
    /// ```
    pub fn new() -> Arc<Hub> {
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let hub = Arc::new(Hub {
            epoch: format!("{epoch:x}-{}", std::process::id()),
            key: random_key(),
            state: Mutex::default(),
        });
        let weak = Arc::downgrade(&hub);
        let _ = std::thread::Builder::new()
            .name("zapline-dev-ping".into())
            .spawn(move || ping(weak));
        hub
    }

    fn lock(&self) -> MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The random key sent to the app with every request, in
    /// [`KEY_HEADER`], and given to it in `ZAPLINE_DEV_PROXY_KEY`: the app
    /// trusts the proxy's `X-Forwarded-For` only when the key matches.
    ///
    /// ```
    /// let hub = zapline_cli::dev::proxy::Hub::new();
    /// assert_eq!(hub.key().len(), 32);
    /// assert_ne!(hub.key(), zapline_cli::dev::proxy::Hub::new().key());
    /// ```
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The app requests are forwarded to.
    pub fn upstream(&self) -> Option<SocketAddr> {
        self.lock().upstream
    }

    /// Forward new requests to `addr`; `None` (with the reason) answers
    /// them with a page that waits for the app to come back.
    pub fn set_upstream(&self, addr: Option<SocketAddr>, down: Option<String>) {
        let mut state = self.lock();
        state.upstream = addr;
        state.down = down;
    }

    /// Push `event` to every open page (and remember the badge and overlay
    /// state for pages that open later).
    pub fn send(&self, event: Event) {
        let mut state = self.lock();
        match &event {
            Event::Building => state.building = true,
            Event::Ready => state.building = false,
            Event::Problem(p) => {
                state.building = false;
                state.problem = Some(p.clone());
            }
            Event::Clear => state.problem = None,
            Event::Reload | Event::Css(_) => {}
        }
        let frame = event.frame();
        state
            .clients
            .retain_mut(|c| c.write_all(frame.as_bytes()).is_ok());
    }

    /// The overlay currently shown, if any.
    pub fn problem(&self) -> Option<Problem> {
        self.lock().problem.clone()
    }

    /// How many pages are listening.
    pub fn clients(&self) -> usize {
        self.lock().clients.len()
    }

    /// Start an event stream on `stream`: the response head, the session
    /// epoch, then the current badge and overlay state.
    fn subscribe(&self, mut stream: TcpStream) {
        let mut state = self.lock();
        let mut out = String::from(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\
             x-accel-buffering: no\r\n\r\nretry: 500\n\n",
        );
        out.push_str(&frame("hello", &json!({ "epoch": self.epoch })));
        if state.building {
            out.push_str(&Event::Building.frame());
        }
        if let Some(p) = &state.problem {
            out.push_str(&Event::Problem(p.clone()).frame());
        }
        let _ = stream.set_write_timeout(Some(EVENT_WRITE_TIMEOUT));
        if stream.write_all(out.as_bytes()).is_ok() {
            state.clients.push(stream);
        }
    }

    fn placeholder(&self) -> (u16, String) {
        let state = self.lock();
        let (status, title, text) = if state.building && state.upstream.is_none() {
            (
                503,
                "Building\u{2026}",
                "zapline dev is building your app. This page reloads when it is ready.".to_owned(),
            )
        } else if let Some(p) = &state.problem {
            (
                503,
                p.title.as_str(),
                format!("{} This page reloads when it is fixed.", p.note),
            )
        } else {
            let why = state
                .down
                .clone()
                .unwrap_or_else(|| "The app is not running.".to_owned());
            (
                502,
                "The app is not running",
                format!("{why} Save a file to rebuild; this page reloads when it is back."),
            )
        };
        let page = format!(
            "<!doctype html><html><head><meta charset=utf-8><title>{t} \u{b7} zapline dev</title>\
             <meta name=viewport content=\"width=device-width\"></head>\
             <body style=\"font:15px/1.5 system-ui,sans-serif;margin:12vh auto;max-width:36em;padding:0 16px;color:#333\">\
             <h1 style=\"font-size:20px\">{t}</h1><p>{x}</p></body></html>",
            t = escape(title),
            x = escape(&text),
        );
        (status, inject::inject(&page, None))
    }
}

fn ping(hub: Weak<Hub>) {
    loop {
        std::thread::sleep(PING);
        let Some(hub) = hub.upgrade() else { return };
        hub.lock()
            .clients
            .retain_mut(|c| c.write_all(b": ping\n\n").is_ok());
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---- the listener ------------------------------------------------------------------------

/// A running proxy. Dropping it stops accepting connections (open ones
/// finish on their own).
#[derive(Debug)]
pub struct Proxy {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl Proxy {
    /// Listens on `addr` and serves connections on background threads.
    ///
    /// ```no_run
    /// use zapline_cli::dev::proxy::{Hub, Proxy};
    /// let hub = Hub::new();
    /// let proxy = Proxy::bind("127.0.0.1:8080".parse().unwrap(), hub.clone())?;
    /// hub.set_upstream(Some("127.0.0.1:41234".parse().unwrap()), None);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn bind(addr: SocketAddr, hub: Arc<Hub>) -> io::Result<Proxy> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let accept = std::thread::Builder::new()
            .name("zapline-dev-proxy".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    if stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(conn) = conn else { continue };
                    let hub = hub.clone();
                    let _ = std::thread::Builder::new()
                        .name("zapline-dev-conn".into())
                        .spawn(move || handle(conn, &hub));
                }
            })?;
        Ok(Proxy {
            addr,
            stop,
            accept: Some(accept),
        })
    }

    /// The address it listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag, and wait until it has
        // closed the listener: the port is free once this returns.
        let woken = TcpStream::connect_timeout(&connectable(self.addr), Duration::from_millis(500));
        if let (Ok(_), Some(accept)) = (woken, self.accept.take()) {
            let _ = accept.join();
        }
    }
}

/// `addr` with an unspecified IP (`0.0.0.0`, `::`) replaced by loopback, so
/// it can be connected to.
pub fn connectable(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(a) if a.ip().is_unspecified() => (Ipv4Addr::LOCALHOST, a.port()).into(),
        SocketAddr::V6(a) if a.ip().is_unspecified() => (Ipv6Addr::LOCALHOST, a.port()).into(),
        a => a,
    }
}

/// Whether an HTTP server answers at `addr` within `timeout`: any status
/// line counts (the probe asks for a path no app has).
pub fn probe(addr: SocketAddr, timeout: Duration) -> bool {
    let attempt = || -> io::Result<bool> {
        let mut conn = TcpStream::connect_timeout(&addr, timeout)?;
        conn.set_read_timeout(Some(timeout))?;
        conn.set_write_timeout(Some(timeout))?;
        conn.write_all(
            b"GET /__zapline/health HTTP/1.1\r\nhost: localhost\r\nuser-agent: zapline-dev\r\nconnection: close\r\n\r\n",
        )?;
        let mut buf = [0u8; 5];
        conn.read_exact(&mut buf)?;
        Ok(&buf == b"HTTP/")
    };
    attempt().unwrap_or(false)
}

// ---- HTTP/1.1 ----------------------------------------------------------------------------

/// A request or response head: the first line and the header fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// `GET /path HTTP/1.1` or `HTTP/1.1 200 OK`.
    pub line: String,
    /// Header fields in order, names as received.
    pub headers: Vec<(String, String)>,
}

impl Head {
    /// The first value of header `name` (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether a comma-separated header `name` lists `token`
    /// (case-insensitive), e.g. `Connection: keep-alive, Upgrade`.
    pub fn has_token(&self, name: &str, token: &str) -> bool {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .flat_map(|(_, v)| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case(token))
    }

    fn content_length(&self) -> Option<u64> {
        self.get("content-length")
            .and_then(|v| v.trim().parse().ok())
    }

    fn chunked(&self) -> bool {
        self.has_token("transfer-encoding", "chunked")
    }

    /// The head as bytes, with fields named in `drop` left out and `extra`
    /// fields appended.
    fn encode(&self, drop: &[&str], extra: &[(&str, &str)]) -> Vec<u8> {
        let mut out = String::with_capacity(256);
        out.push_str(&self.line);
        out.push_str("\r\n");
        for (n, v) in &self.headers {
            if !drop.iter().any(|d| n.eq_ignore_ascii_case(d)) {
                out.push_str(n);
                out.push_str(": ");
                out.push_str(v);
                out.push_str("\r\n");
            }
        }
        for (n, v) in extra {
            out.push_str(n);
            out.push_str(": ");
            out.push_str(v);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out.into_bytes()
    }
}

/// Reads one head. `Ok(None)` when the peer closed the connection before
/// sending anything.
pub fn read_head(reader: &mut impl BufRead) -> io::Result<Option<Head>> {
    let mut total = 0;
    let mut line = Vec::new();
    let mut first: Option<String> = None;
    let mut headers = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            return match first {
                None => Ok(None),
                Some(_) => Err(io::ErrorKind::UnexpectedEof.into()),
            };
        }
        total += n;
        if total > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "head too large"));
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        match &first {
            // Tolerate blank lines before a request (RFC 9112 2.2).
            None if text.is_empty() => continue,
            None => first = Some(text.to_owned()),
            Some(_) if text.is_empty() => break,
            Some(_) => {
                if let Some((name, value)) = text.split_once(':') {
                    headers.push((name.trim().to_owned(), value.trim().to_owned()));
                }
            }
        }
    }
    Ok(first.map(|line| Head { line, headers }))
}

/// Copies a chunked body verbatim (sizes, data and trailers) from `from` to
/// `to`, stopping after its last chunk.
fn copy_chunked(from: &mut impl BufRead, to: &mut impl Write) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if from.read_until(b'\n', &mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        to.write_all(&line)?;
        let size = chunk_size(&line)?;
        if size == 0 {
            // Trailers, up to the blank line.
            loop {
                line.clear();
                if from.read_until(b'\n', &mut line)? == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                to.write_all(&line)?;
                if line == b"\r\n" || line == b"\n" {
                    return to.flush();
                }
            }
        }
        copy_exact(from, to, size + 2)?;
        to.flush()?;
    }
}

/// A response body read piece by piece, without its framing.
enum BodyReader {
    /// `left`: bytes of the current chunk not read yet.
    Chunked { left: u64, done: bool },
    /// Bytes left.
    Length(u64),
    /// The body ends when the app closes the connection.
    Close { done: bool },
}

impl BodyReader {
    fn new(res: &Head) -> BodyReader {
        if res.chunked() {
            BodyReader::Chunked {
                left: 0,
                done: false,
            }
        } else if let Some(n) = res.content_length() {
            BodyReader::Length(n)
        } else {
            BodyReader::Close { done: false }
        }
    }

    /// The next piece of the body (at most [`PIECE`] bytes), `None` at its
    /// end.
    fn next(&mut self, from: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
        match self {
            BodyReader::Chunked { done: true, .. } | BodyReader::Close { done: true } => Ok(None),
            BodyReader::Chunked { left, done } => {
                if *left == 0 {
                    let mut line = Vec::new();
                    if from.read_until(b'\n', &mut line)? == 0 {
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    let size = chunk_size(&line)?;
                    if size == 0 {
                        // Trailers, up to the blank line.
                        loop {
                            line.clear();
                            if from.read_until(b'\n', &mut line)? == 0
                                || line == b"\r\n"
                                || line == b"\n"
                            {
                                *done = true;
                                return Ok(None);
                            }
                        }
                    }
                    *left = size;
                }
                let mut piece = vec![0; (*left).min(PIECE) as usize];
                from.read_exact(&mut piece)?;
                *left -= piece.len() as u64;
                if *left == 0 {
                    let mut crlf = [0u8; 2];
                    from.read_exact(&mut crlf)?;
                }
                Ok(Some(piece))
            }
            BodyReader::Length(left) => {
                if *left == 0 {
                    return Ok(None);
                }
                let mut piece = Vec::new();
                from.take((*left).min(PIECE)).read_to_end(&mut piece)?;
                if piece.is_empty() {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                *left -= piece.len() as u64;
                Ok(Some(piece))
            }
            BodyReader::Close { done } => {
                let mut piece = Vec::new();
                from.take(PIECE).read_to_end(&mut piece)?;
                if piece.is_empty() {
                    *done = true;
                    return Ok(None);
                }
                Ok(Some(piece))
            }
        }
    }
}

/// Whether more of the response is ready within `wait` (or the app closed
/// the connection): `false` when the app paused.
fn arrives_within(from: &mut BufReader<TcpStream>, wait: Duration) -> io::Result<bool> {
    if !from.buffer().is_empty() {
        return Ok(true);
    }
    from.get_ref().set_read_timeout(Some(wait))?;
    let ready = from.fill_buf().map(|_| ());
    from.get_ref().set_read_timeout(None)?;
    match ready {
        Ok(()) => Ok(true),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Writes `data` as one chunk (nothing for empty data, which would end the
/// body).
fn write_chunk(to: &mut impl Write, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write!(to, "{:x}\r\n", data.len())?;
    to.write_all(data)?;
    to.write_all(b"\r\n")
}

fn chunk_size(line: &[u8]) -> io::Result<u64> {
    let text = String::from_utf8_lossy(line);
    let hex = text.split(';').next().unwrap_or("").trim();
    u64::from_str_radix(hex, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))
}

fn copy_exact(from: &mut impl Read, to: &mut impl Write, n: u64) -> io::Result<()> {
    let copied = io::copy(&mut from.take(n), to)?;
    if copied < n {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// Copies until `from` ends, flushing after every read so streamed
/// responses (server-sent events) arrive as they are written.
fn copy_streaming(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return to.flush(),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&buf[..n])?;
        to.flush()?;
    }
}

/// Like [`copy_exact`], flushing as data arrives.
fn copy_exact_streaming(from: &mut impl Read, to: &mut impl Write, n: u64) -> io::Result<()> {
    let mut limited = from.take(n);
    copy_streaming(&mut limited, to)?;
    if limited.limit() > 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// Fields that describe one connection, not the message (RFC 9110 7.6.1),
/// plus the framing the proxy redoes itself.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "te",
    "trailer",
    "upgrade",
];

// ---- one connection ----------------------------------------------------------------------

/// What to do with the browser's connection after a response.
#[derive(Debug, PartialEq, Eq)]
enum After {
    KeepAlive,
    Close,
    /// Someone else (the hub) owns the connection now.
    Detached,
}

fn handle(client: TcpStream, hub: &Hub) {
    let _ = client.set_nodelay(true);
    let _ = client.set_read_timeout(Some(IDLE));
    let peer = client.peer_addr().ok();
    let Ok(read_half) = client.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = client;
    loop {
        let head = match read_head(&mut reader) {
            Ok(Some(head)) => head,
            Ok(None) | Err(_) => return,
        };
        match serve(head, &mut reader, &mut writer, hub, peer) {
            Ok(After::KeepAlive) => {}
            Ok(After::Detached) => return,
            Ok(After::Close) | Err(_) => {
                let _ = writer.shutdown(Shutdown::Write);
                return;
            }
        }
    }
}

fn serve(
    req: Head,
    reader: &mut BufReader<TcpStream>,
    client: &mut TcpStream,
    hub: &Hub,
    peer: Option<SocketAddr>,
) -> io::Result<After> {
    let mut parts = req.line.split_ascii_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("/").to_owned();
    let version = parts.next().unwrap_or("HTTP/1.1");
    let path = target.split(['?', '#']).next().unwrap_or("");
    let keep_alive = if version.eq_ignore_ascii_case("HTTP/1.0") {
        req.has_token("connection", "keep-alive")
    } else {
        !req.has_token("connection", "close")
    };
    let after = if keep_alive {
        After::KeepAlive
    } else {
        After::Close
    };

    if path == inject::EVENTS_PATH {
        discard_body(&req, reader)?;
        hub.subscribe(client.try_clone()?);
        // The hub owns the stream now; this thread is done with it.
        return Ok(After::Detached);
    }
    if path == inject::CLIENT_PATH {
        discard_body(&req, reader)?;
        respond(
            client,
            200,
            "application/javascript; charset=utf-8",
            inject::CLIENT_JS.as_bytes(),
            &after,
        )?;
        return Ok(after);
    }
    if path.starts_with(inject::PREFIX) {
        discard_body(&req, reader)?;
        respond(
            client,
            404,
            "text/plain; charset=utf-8",
            b"not found",
            &after,
        )?;
        return Ok(after);
    }

    let upstream = hub
        .upstream()
        .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok());
    let Some(upstream) = upstream else {
        discard_body(&req, reader)?;
        let (status, page) = hub.placeholder();
        respond(
            client,
            status,
            "text/html; charset=utf-8",
            page.as_bytes(),
            &after,
        )?;
        return Ok(after);
    };
    let _ = upstream.set_nodelay(true);
    forward(
        &req,
        &method,
        reader,
        client,
        upstream,
        peer,
        keep_alive,
        !version.eq_ignore_ascii_case("HTTP/1.0"),
        hub.key(),
    )
}

/// Sends a small complete response.
fn respond(
    client: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    after: &After,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    };
    let connection = if *after == After::Close {
        "connection: close\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
         cache-control: no-store\r\n{connection}\r\n",
        body.len()
    );
    client.write_all(head.as_bytes())?;
    client.write_all(body)?;
    client.flush()
}

/// Reads and drops a request body the proxy answers itself.
fn discard_body(req: &Head, reader: &mut BufReader<TcpStream>) -> io::Result<()> {
    if req.chunked() {
        copy_chunked(reader, &mut io::sink())
    } else if let Some(n) = req.content_length() {
        copy_exact(reader, &mut io::sink(), n)
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn forward(
    req: &Head,
    method: &str,
    reader: &mut BufReader<TcpStream>,
    client: &mut TcpStream,
    upstream: TcpStream,
    peer: Option<SocketAddr>,
    keep_alive: bool,
    http11: bool,
    key: &str,
) -> io::Result<After> {
    let upgrade = req
        .get("upgrade")
        .filter(|_| req.has_token("connection", "upgrade"));
    // Pages are buffered to add the client; ask for them uncompressed.
    let wants_html = req
        .get("accept")
        .is_some_and(|a| a.to_ascii_lowercase().contains("text/html"));
    let mut drop: Vec<&str> = HOP_BY_HOP.to_vec();
    drop.push("expect");
    drop.push("x-forwarded-for");
    drop.push(KEY_HEADER);
    if wants_html {
        drop.push("accept-encoding");
    }
    let forwarded_for = {
        let ip = peer.map(|p| p.ip().to_string()).unwrap_or_default();
        match req.get("x-forwarded-for") {
            Some(prior) if !ip.is_empty() => format!("{prior}, {ip}"),
            Some(prior) => prior.to_owned(),
            None => ip,
        }
    };
    let mut extra: Vec<(&str, &str)> = Vec::new();
    match upgrade {
        Some(proto) => {
            extra.push(("connection", "upgrade"));
            extra.push(("upgrade", proto));
        }
        None => extra.push(("connection", "close")),
    }
    if !forwarded_for.is_empty() {
        extra.push(("x-forwarded-for", &forwarded_for));
    }
    extra.push((KEY_HEADER, key));
    if req.get("x-forwarded-proto").is_none() {
        extra.push(("x-forwarded-proto", "http"));
    }
    let host = req.get("host").map(str::to_owned);
    if let (Some(host), None) = (&host, req.get("x-forwarded-host")) {
        extra.push(("x-forwarded-host", host));
    }

    if req
        .get("expect")
        .is_some_and(|e| e.eq_ignore_ascii_case("100-continue"))
    {
        client.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let mut up_writer = upstream.try_clone()?;
    up_writer.write_all(&req.encode(&drop, &extra))?;
    if req.chunked() {
        copy_chunked(reader, &mut up_writer)?;
    } else if let Some(n) = req.content_length() {
        copy_exact(reader, &mut up_writer, n)?;
    }
    up_writer.flush()?;

    let mut up_reader = BufReader::new(upstream);
    let res = loop {
        let res = read_head(&mut up_reader)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        let status = status_of(&res);
        // Interim responses (103 Early Hints...) pass straight through.
        if (100..200).contains(&status) && status != 101 {
            client.write_all(&res.encode(&[], &[]))?;
            continue;
        }
        break res;
    };
    let status = status_of(&res);

    if status == 101 && upgrade.is_some() {
        client.write_all(&res.encode(&[], &[]))?;
        tunnel(reader, client, up_reader, up_writer);
        return Ok(After::Close);
    }

    let no_body = method.eq_ignore_ascii_case("HEAD") || status == 204 || status == 304;
    let content_type = res.get("content-type").unwrap_or("");
    let encoded = res
        .get("content-encoding")
        .is_some_and(|e| !e.trim().eq_ignore_ascii_case("identity"));
    let keep = if keep_alive {
        After::KeepAlive
    } else {
        After::Close
    };
    let close_field: &[(&str, &str)] = if keep_alive {
        &[]
    } else {
        &[("connection", "close")]
    };

    if !no_body && !encoded && inject::is_html(content_type) {
        let nonce = res
            .get("content-security-policy")
            .and_then(inject::csp_nonce);
        let mut body = BodyReader::new(&res);
        let mut buffered = Vec::new();
        // Read the page while it keeps coming, up to `MAX_INJECT`.
        let whole = loop {
            if buffered.len() > MAX_INJECT || !arrives_within(&mut up_reader, HTML_PAUSE)? {
                break false;
            }
            match body.next(&mut up_reader)? {
                Some(piece) => buffered.extend_from_slice(&piece),
                None => break true,
            }
        };
        let mut drop = HOP_BY_HOP.to_vec();
        drop.extend(["content-length", "transfer-encoding"]);
        if whole {
            let page = match String::from_utf8(buffered) {
                Ok(html) => inject::inject(&html, nonce.as_deref()).into_bytes(),
                Err(e) => e.into_bytes(),
            };
            let length = page.len().to_string();
            let mut fields = vec![("content-length", length.as_str())];
            fields.extend_from_slice(close_field);
            client.write_all(&res.encode(&drop, &fields))?;
            client.write_all(&page)?;
            client.flush()?;
            return Ok(keep);
        }
        // The app streams the page: forward it as it arrives, re-chunked
        // (or ended by closing for an HTTP/1.0 browser), with the client
        // added before `</head>` (or after `<body>`) when that part passes,
        // else at the end.
        let (fields, after): (Vec<(&str, &str)>, After) = if http11 {
            let mut fields = vec![("transfer-encoding", "chunked")];
            fields.extend_from_slice(close_field);
            (fields, keep)
        } else {
            (vec![("connection", "close")], After::Close)
        };
        client.write_all(&res.encode(&drop, &fields))?;
        let tag = inject::script_tag(nonce.as_deref());
        let mut page = inject::is_page(&buffered);
        let mut done = inject::loads_client(&buffered);
        let mut piece = buffered;
        loop {
            let mut out = piece;
            if !done && page {
                if let Some(at) = inject::early_insertion_point(&out) {
                    out.splice(at..at, tag.bytes());
                    done = true;
                }
            }
            if http11 {
                write_chunk(client, &out)?;
            } else {
                client.write_all(&out)?;
            }
            client.flush()?;
            match body.next(&mut up_reader)? {
                Some(next) => {
                    page |= inject::is_page(&next);
                    done |= inject::loads_client(&next);
                    piece = next;
                }
                None => break,
            }
        }
        if !done && page {
            if http11 {
                write_chunk(client, tag.as_bytes())?;
            } else {
                client.write_all(tag.as_bytes())?;
            }
        }
        if http11 {
            client.write_all(b"0\r\n\r\n")?;
        }
        client.flush()?;
        return Ok(after);
    }

    if no_body {
        client.write_all(&res.encode(HOP_BY_HOP, close_field))?;
        client.flush()?;
        return Ok(keep);
    }
    if res.chunked() {
        client.write_all(&res.encode(HOP_BY_HOP, close_field))?;
        copy_chunked(&mut up_reader, client)?;
        return Ok(keep);
    }
    if let Some(n) = res.content_length() {
        client.write_all(&res.encode(HOP_BY_HOP, close_field))?;
        copy_exact_streaming(&mut up_reader, client, n)?;
        return Ok(keep);
    }
    // No framing: the body ends when the app closes, so must ours.
    client.write_all(&res.encode(HOP_BY_HOP, &[("connection", "close")]))?;
    copy_streaming(&mut up_reader, client)?;
    Ok(After::Close)
}

fn status_of(res: &Head) -> u16 {
    res.line
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Pipes bytes both ways after a protocol switch, until either side closes.
fn tunnel(
    client_reader: &mut BufReader<TcpStream>,
    client: &mut TcpStream,
    up_reader: BufReader<TcpStream>,
    mut up_writer: TcpStream,
) {
    let _ = client.set_read_timeout(None);
    // Bytes already buffered on either side go first.
    let pending_up = client_reader.buffer().to_vec();
    let pending_down = up_reader.buffer().to_vec();
    let (Ok(mut client_in), Ok(mut client_out)) = (client.try_clone(), client.try_clone()) else {
        return;
    };
    let up_shutdown = up_writer.try_clone().ok();
    let to_app = std::thread::spawn(move || {
        if up_writer.write_all(&pending_up).is_ok() {
            let _ = io::copy(&mut client_in, &mut up_writer);
        }
        let _ = up_writer.shutdown(Shutdown::Write);
    });
    if client_out.write_all(&pending_down).is_ok() {
        let _ = io::copy(&mut up_reader.into_inner(), &mut client_out);
    }
    let _ = client_out.shutdown(Shutdown::Both);
    if let Some(up) = up_shutdown {
        let _ = up.shutdown(Shutdown::Both);
    }
    let _ = to_app.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reads_heads_and_their_fields() {
        let mut input = Cursor::new(
            b"\r\nGET /a?b=1 HTTP/1.1\r\nHost: x\r\nConnection: keep-alive, Upgrade\r\n\r\nrest"
                .to_vec(),
        );
        let head = read_head(&mut input).unwrap().unwrap();
        assert_eq!(head.line, "GET /a?b=1 HTTP/1.1");
        assert_eq!(head.get("host"), Some("x"));
        assert!(head.has_token("connection", "upgrade"));
        assert!(!head.has_token("connection", "close"));
        let mut rest = String::new();
        input.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "rest");

        assert_eq!(read_head(&mut Cursor::new(Vec::new())).unwrap(), None);
        assert!(read_head(&mut Cursor::new(b"GET / HTTP/1.1\r\nHost".to_vec())).is_err());
    }

    #[test]
    fn rejects_huge_heads() {
        let mut big = b"GET / HTTP/1.1\r\n".to_vec();
        for _ in 0..2000 {
            big.extend_from_slice(b"x-filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
        }
        assert!(read_head(&mut Cursor::new(big)).is_err());
    }

    #[test]
    fn encodes_heads_dropping_and_adding_fields() {
        let head = Head {
            line: "HTTP/1.1 200 OK".into(),
            headers: vec![
                ("Content-Length".into(), "3".into()),
                ("Connection".into(), "keep-alive".into()),
            ],
        };
        let out = head.encode(&["connection"], &[("x-a", "1")]);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nx-a: 1\r\n\r\n"
        );
    }

    fn body_of(input: &mut Cursor<Vec<u8>>) -> io::Result<Vec<u8>> {
        let mut body = BodyReader::Chunked {
            left: 0,
            done: false,
        };
        let mut all = Vec::new();
        while let Some(piece) = body.next(input)? {
            all.extend(piece);
        }
        Ok(all)
    }

    #[test]
    fn copies_and_decodes_chunked_bodies() {
        let raw = b"4\r\nWiki\r\n6;ext=1\r\npedia \r\n0\r\nx-trailer: y\r\n\r\nNEXT";
        let mut out = Vec::new();
        let mut input = Cursor::new(raw.to_vec());
        copy_chunked(&mut input, &mut out).unwrap();
        assert_eq!(out, &raw[..raw.len() - 4]);

        let mut input = Cursor::new(raw.to_vec());
        let mut body = BodyReader::Chunked {
            left: 0,
            done: false,
        };
        let mut pieces = Vec::new();
        while let Some(piece) = body.next(&mut input).unwrap() {
            pieces.push(String::from_utf8(piece).unwrap());
        }
        assert_eq!(pieces, ["Wiki", "pedia "]);
        assert_eq!(input.position() as usize, raw.len() - 4);
        let mut truncated = Cursor::new(b"4\r\nWi".to_vec());
        assert!(body_of(&mut truncated).is_err());
        let mut truncated = Cursor::new(b"4\r\nWi".to_vec());
        assert!(copy_chunked(&mut truncated, &mut Vec::new()).is_err());
    }

    #[test]
    fn frames_events() {
        assert_eq!(
            Event::Css(vec!["static/app.css".into()]).frame(),
            "event: css\ndata: {\"paths\":[\"static/app.css\"]}\n\n"
        );
        let problem = Problem {
            title: "Build failed".into(),
            note: "1 error".into(),
            errors: vec![Diagnostic {
                level: "error".into(),
                message: "bad\nthing".into(),
                file: Some("src/main.rs".into()),
                line: 2,
                column: 5,
                rendered: "\x1b[31merror\x1b[0m: bad".into(),
            }],
        };
        let frame = Event::Problem(problem).frame();
        assert!(frame.starts_with("event: problem\ndata: {"), "{frame}");
        assert_eq!(frame.matches('\n').count(), 3, "one data line: {frame}");
        let data: serde_json::Value = serde_json::from_str(
            frame
                .lines()
                .nth(1)
                .unwrap()
                .strip_prefix("data: ")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(data["errors"][0]["file"], "src/main.rs");
        assert_eq!(data["errors"][0]["line"], 2);
        assert_eq!(data["errors"][0]["rendered"], "error: bad");
    }

    #[test]
    fn unspecified_addresses_become_loopback() {
        assert_eq!(
            connectable("0.0.0.0:80".parse().unwrap()).to_string(),
            "127.0.0.1:80"
        );
        assert_eq!(
            connectable("[::]:80".parse().unwrap()).to_string(),
            "[::1]:80"
        );
        assert_eq!(
            connectable("10.1.2.3:80".parse().unwrap()).to_string(),
            "10.1.2.3:80"
        );
    }
}
