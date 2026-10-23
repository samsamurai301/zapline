//! The `zapline dev` proxy against a local test server: live reload
//! injection, pass-through of everything else, upgrades, the event stream
//! and switching to a new app.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zapline_cli::build::Diagnostic;
use zapline_cli::dev::proxy::{probe, read_head, Event, Head, Hub, Problem, Proxy};

const TAG: &str = r#"<script src="/__zapline/client.js" defer></script>"#;

// ---- a tiny upstream app -----------------------------------------------------------------

/// Serves one request per connection (the proxy always asks for
/// `Connection: close`), named `version` in its pages.
fn upstream(version: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            std::thread::spawn(move || {
                let _ = app(conn, version);
            });
        }
    });
    addr
}

fn app(conn: TcpStream, version: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut out = conn;
    let Some(req) = read_head(&mut reader)? else {
        return Ok(());
    };
    let path = req.line.split(' ').nth(1).unwrap_or("/").to_owned();
    let body = read_body(&req, &mut reader)?;
    let page = format!("<html><head><title>{version}</title></head><body>{version}</body></html>");
    match path.as_str() {
        "/page" => send(
            &mut out,
            "200 OK",
            "text/html; charset=utf-8",
            &[],
            page.as_bytes(),
        ),
        "/chunked" => {
            out.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n")?;
            for part in ["<html><head></head>", "<body>chunked</body></html>"] {
                write!(out, "{:x}\r\n{part}\r\n", part.len())?;
            }
            out.write_all(b"0\r\n\r\n")
        }
        "/json" => send(
            &mut out,
            "200 OK",
            "application/json",
            &[],
            br#"{"head":"</head>"}"#,
        ),
        "/csp" => send(
            &mut out,
            "200 OK",
            "text/html",
            &[("content-security-policy", "script-src 'nonce-n0nce'")],
            b"<head></head>",
        ),
        "/gzip" => send(
            &mut out,
            "200 OK",
            "text/html",
            &[("content-encoding", "gzip")],
            b"\x1f\x8b not really",
        ),
        "/echo" => send(&mut out, "200 OK", "application/octet-stream", &[], &body),
        "/headers" => {
            let text: String = req
                .headers
                .iter()
                .map(|(n, v)| format!("{}: {v}\n", n.to_ascii_lowercase()))
                .collect();
            send(&mut out, "200 OK", "text/plain", &[], text.as_bytes())
        }
        "/stream" => {
            // No length, no chunking: the body ends when the app closes.
            out.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")?;
            out.write_all(b"data: one\n\n")?;
            out.flush()?;
            std::thread::sleep(Duration::from_millis(200));
            out.write_all(b"data: two\n\n")
        }
        "/empty" => out.write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n"),
        "/ws" => {
            out.write_all(b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: upgrade\r\n\r\n")?;
            // Echo every byte until the client closes.
            let mut buf = [0u8; 1024];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                out.write_all(&buf[..n])?;
            }
        }
        _ => send(&mut out, "404 Not Found", "text/plain", &[], b"nope"),
    }
}

fn send(
    out: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (n, v) in extra {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    out.write_all(body)
}

/// A request or response body, decoded.
fn read_body(head: &Head, reader: &mut impl BufRead) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    if head
        .get("transfer-encoding")
        .is_some_and(|t| t.contains("chunked"))
    {
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
            if size == 0 {
                let mut end = String::new();
                reader.read_line(&mut end)?;
                return Ok(body);
            }
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk[..size]);
        }
    } else if let Some(n) = head.get("content-length") {
        body.resize(n.parse().unwrap(), 0);
        reader.read_exact(&mut body)?;
    }
    Ok(body)
}

// ---- a browser-ish client ------------------------------------------------------------------

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(addr: SocketAddr) -> Client {
        let conn = TcpStream::connect(addr).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Client {
            reader: BufReader::new(conn.try_clone().unwrap()),
            writer: conn,
        }
    }

    fn send(&mut self, raw: &str) {
        self.writer.write_all(raw.as_bytes()).unwrap();
    }

    /// Sends a GET and reads the whole response.
    fn get(&mut self, path: &str, accept: &str) -> (Head, String) {
        self.send(&format!(
            "GET {path} HTTP/1.1\r\nhost: localhost\r\naccept: {accept}\r\naccept-encoding: gzip, br\r\n\r\n"
        ));
        self.response(false)
    }

    fn response(&mut self, head_only: bool) -> (Head, String) {
        let head = read_head(&mut self.reader).unwrap().expect("a response");
        let status = head.line.split(' ').nth(1).unwrap_or("");
        if head_only || status == "204" || status == "304" {
            return (head, String::new());
        }
        let body = if head.get("content-length").is_some()
            || head
                .get("transfer-encoding")
                .is_some_and(|t| t.contains("chunked"))
        {
            read_body(&head, &mut self.reader).unwrap()
        } else {
            let mut all = Vec::new();
            self.reader.read_to_end(&mut all).unwrap();
            all
        };
        (head, String::from_utf8_lossy(&body).into_owned())
    }
}

fn proxy_to(up: Option<SocketAddr>) -> (Arc<Hub>, Proxy) {
    let hub = Hub::new();
    let proxy = Proxy::bind("127.0.0.1:0".parse().unwrap(), hub.clone()).unwrap();
    hub.set_upstream(up, None);
    (hub, proxy)
}

const HTML: &str = "text/html,application/xhtml+xml,*/*;q=0.8";

// ---- tests ---------------------------------------------------------------------------------

#[test]
fn html_pages_get_the_client_and_a_correct_length_over_keep_alive() {
    let (_hub, proxy) = proxy_to(Some(upstream("v1")));
    let mut c = Client::connect(proxy.addr());
    for _ in 0..3 {
        let (head, body) = c.get("/page", HTML);
        assert!(head.line.starts_with("HTTP/1.1 200"), "{head:?}");
        assert_eq!(
            body,
            format!("<html><head><title>v1</title>{TAG}</head><body>v1</body></html>")
        );
        assert_eq!(
            head.get("content-length"),
            Some(body.len().to_string().as_str())
        );
        // The app's `connection: close` is not passed on: the browser keeps
        // its connection.
        assert_eq!(head.get("connection"), None);
    }
}

#[test]
fn chunked_pages_are_decoded_and_injected() {
    let (_hub, proxy) = proxy_to(Some(upstream("v1")));
    let (head, body) = Client::connect(proxy.addr()).get("/chunked", HTML);
    assert_eq!(
        body,
        format!("<html><head>{TAG}</head><body>chunked</body></html>")
    );
    assert_eq!(head.get("transfer-encoding"), None);
    assert_eq!(
        head.get("content-length"),
        Some(body.len().to_string().as_str())
    );
}

#[test]
fn other_responses_pass_through_untouched() {
    let (_hub, proxy) = proxy_to(Some(upstream("v1")));
    let mut c = Client::connect(proxy.addr());
    let (head, body) = c.get("/json", "application/json");
    assert_eq!(body, r#"{"head":"</head>"}"#);
    assert_eq!(head.get("content-type"), Some("application/json"));

    let (head, body) = c.get("/gzip", HTML);
    assert_eq!(head.get("content-encoding"), Some("gzip"));
    assert!(!body.contains("__zapline"), "{body}");

    let (head, _) = c.get("/missing", HTML);
    assert!(head.line.starts_with("HTTP/1.1 404"), "{head:?}");

    let (head, body) = c.get("/empty", HTML);
    assert!(head.line.starts_with("HTTP/1.1 204"), "{head:?}");
    assert_eq!(body, "");
    // Still usable after a bodiless response.
    let (_, body) = c.get("/json", "*/*");
    assert!(body.starts_with('{'));
}

#[test]
fn the_csp_nonce_is_reused() {
    let (_hub, proxy) = proxy_to(Some(upstream("v1")));
    let (_, body) = Client::connect(proxy.addr()).get("/csp", HTML);
    assert_eq!(
        body,
        r#"<head><script src="/__zapline/client.js" defer nonce="n0nce"></script></head>"#
    );
}

#[test]
fn request_bodies_and_headers_reach_the_app() {
    let (_hub, proxy) = proxy_to(Some(upstream("v1")));
    let mut c = Client::connect(proxy.addr());
    c.send("POST /echo HTTP/1.1\r\nhost: x\r\ncontent-length: 5\r\n\r\nhello");
    assert_eq!(c.response(false).1, "hello");
    c.send("POST /echo HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n");
    assert_eq!(c.response(false).1, "abcde");
    // `Expect: 100-continue` is answered by the proxy.
    c.send("POST /echo HTTP/1.1\r\nhost: x\r\ncontent-length: 2\r\nexpect: 100-continue\r\n\r\n");
    let (interim, _) = c.response(true);
    assert!(interim.line.starts_with("HTTP/1.1 100"), "{interim:?}");
    c.send("ok");
    assert_eq!(c.response(false).1, "ok");

    let (_, seen) = c.get("/headers", HTML);
    assert!(seen.contains("host: localhost\n"), "{seen}");
    assert!(seen.contains("connection: close\n"), "{seen}");
    assert!(seen.contains("x-forwarded-for: 127.0.0.1\n"), "{seen}");
    assert!(seen.contains("x-forwarded-proto: http\n"), "{seen}");
    // Pages are fetched uncompressed so the client can be added...
    assert!(!seen.contains("accept-encoding"), "{seen}");
    // ...but other requests keep their encodings.
    let (_, seen) = c.get("/headers", "application/json");
    assert!(seen.contains("accept-encoding: gzip, br\n"), "{seen}");
}

#[test]
fn unframed_streams_flow_through_as_they_are_written() {
    let (_hub, proxy) = proxy_to(Some(upstream("v1")));
    let mut c = Client::connect(proxy.addr());
    c.send("GET /stream HTTP/1.1\r\nhost: x\r\naccept: text/event-stream\r\n\r\n");
    let (head, _) = c.response(true);
    assert_eq!(head.get("connection"), Some("close"));
    let mut first = String::new();
    c.reader.read_line(&mut first).unwrap();
    assert_eq!(first, "data: one\n");
    let mut rest = String::new();
    c.reader.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "\ndata: two\n\n");
}

#[test]
fn upgrades_are_tunnelled() {
    let (_hub, proxy) = proxy_to(Some(upstream("v1")));
    let mut c = Client::connect(proxy.addr());
    c.send("GET /ws HTTP/1.1\r\nhost: x\r\nconnection: Upgrade\r\nupgrade: websocket\r\n\r\n");
    let (head, _) = c.response(true);
    assert!(head.line.starts_with("HTTP/1.1 101"), "{head:?}");
    for msg in ["ping", "pong!"] {
        c.send(msg);
        let mut buf = vec![0u8; msg.len()];
        c.reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, msg.as_bytes());
    }
}

#[test]
fn without_an_app_pages_wait_for_it() {
    let (hub, proxy) = proxy_to(None);
    hub.send(Event::Building);
    let (head, body) = Client::connect(proxy.addr()).get("/page", HTML);
    assert!(head.line.starts_with("HTTP/1.1 503"), "{head:?}");
    assert!(body.contains("Building"), "{body}");
    assert!(body.contains(TAG), "{body}");

    hub.send(Event::Ready);
    hub.set_upstream(None, Some("The app exited with exit status: 3.".into()));
    let (head, body) = Client::connect(proxy.addr()).get("/page", HTML);
    assert!(head.line.starts_with("HTTP/1.1 502"), "{head:?}");
    assert!(body.contains("exit status: 3"), "{body}");

    // An app that is gone (nothing listens on its port) is treated the same.
    let gone = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    hub.set_upstream(Some(gone), None);
    let (head, _) = Client::connect(proxy.addr()).get("/page", HTML);
    assert!(head.line.starts_with("HTTP/1.1 502"), "{head:?}");

    hub.set_upstream(Some(upstream("v1")), None);
    let (head, _) = Client::connect(proxy.addr()).get("/page", HTML);
    assert!(head.line.starts_with("HTTP/1.1 200"), "{head:?}");
}

#[test]
fn requests_follow_the_upstream_switch_immediately() {
    let (hub, proxy) = proxy_to(Some(upstream("v1")));
    let mut c = Client::connect(proxy.addr());
    assert!(c.get("/page", HTML).1.contains("<body>v1</body>"));
    hub.set_upstream(Some(upstream("v2")), None);
    // Same browser connection, new app.
    assert!(c.get("/page", HTML).1.contains("<body>v2</body>"));
}

#[test]
fn serves_the_client_and_owns_its_prefix() {
    let (_hub, proxy) = proxy_to(Some(upstream("v1")));
    let mut c = Client::connect(proxy.addr());
    let (head, js) = c.get("/__zapline/client.js", "*/*");
    assert!(head.line.starts_with("HTTP/1.1 200"), "{head:?}");
    assert!(
        head.get("content-type")
            .is_some_and(|t| t.starts_with("application/javascript")),
        "{head:?}"
    );
    assert!(js.contains("EventSource"));
    let (head, _) = c.get("/__zapline/other", "*/*");
    assert!(head.line.starts_with("HTTP/1.1 404"), "{head:?}");
}

/// Reads server-sent events until one named `name` arrives; returns its data.
fn next_event(reader: &mut BufReader<TcpStream>, name: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut current = String::new();
    loop {
        assert!(Instant::now() < deadline, "no {name} event");
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            panic!("event stream closed before {name}");
        }
        let line = line.trim_end_matches('\n');
        if let Some(event) = line.strip_prefix("event: ") {
            current = event.to_owned();
        } else if let Some(data) = line.strip_prefix("data: ") {
            if current == name {
                return data.to_owned();
            }
        }
    }
}

#[test]
fn event_streams_get_the_current_state_then_live_events() {
    let (hub, proxy) = proxy_to(Some(upstream("v1")));
    hub.send(Event::Problem(Problem {
        title: "Build failed".into(),
        note: "1 error.".into(),
        errors: vec![Diagnostic {
            level: "error".into(),
            message: "expected `;`".into(),
            file: Some("src/main.rs".into()),
            line: 7,
            column: 12,
            rendered: "\x1b[1merror\x1b[0m: expected `;`".into(),
        }],
    }));

    let mut c = Client::connect(proxy.addr());
    c.send("GET /__zapline/events HTTP/1.1\r\nhost: x\r\naccept: text/event-stream\r\n\r\n");
    let (head, _) = c.response(true);
    assert_eq!(head.get("content-type"), Some("text/event-stream"));
    let hello: serde_json::Value =
        serde_json::from_str(&next_event(&mut c.reader, "hello")).unwrap();
    assert!(hello["epoch"].as_str().is_some_and(|e| !e.is_empty()));
    // A page opened while the build is broken shows the overlay at once.
    let problem: serde_json::Value =
        serde_json::from_str(&next_event(&mut c.reader, "problem")).unwrap();
    assert_eq!(problem["errors"][0]["file"], "src/main.rs");
    assert_eq!(problem["errors"][0]["line"], 7);
    assert_eq!(problem["errors"][0]["rendered"], "error: expected `;`");

    let deadline = Instant::now() + Duration::from_secs(5);
    while hub.clients() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(hub.clients(), 1);
    hub.send(Event::Clear);
    next_event(&mut c.reader, "clear");
    assert_eq!(hub.problem(), None);
    hub.send(Event::Css(vec!["static/app.css".into()]));
    assert_eq!(
        next_event(&mut c.reader, "css"),
        r#"{"paths":["static/app.css"]}"#
    );
    hub.send(Event::Reload);
    next_event(&mut c.reader, "reload");

    // A closed page is dropped on the next event.
    drop(c);
    let deadline = Instant::now() + Duration::from_secs(5);
    while hub.clients() > 0 && Instant::now() < deadline {
        hub.send(Event::Ready);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(hub.clients(), 0);
}

#[test]
fn probe_tells_an_http_server_from_a_silent_port() {
    assert!(probe(upstream("v1"), Duration::from_secs(2)));
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    assert!(!probe(
        silent.local_addr().unwrap(),
        Duration::from_millis(200)
    ));
    let closed = silent.local_addr().unwrap();
    drop(silent);
    assert!(!probe(closed, Duration::from_millis(200)));
}

#[test]
fn dropping_the_proxy_frees_its_port() {
    let hub = Hub::new();
    let proxy = Proxy::bind("127.0.0.1:0".parse().unwrap(), hub.clone()).unwrap();
    let addr = proxy.addr();
    drop(proxy);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Proxy::bind(addr, hub.clone()) {
            Ok(_) => break,
            Err(e) if Instant::now() > deadline => panic!("port still taken: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
