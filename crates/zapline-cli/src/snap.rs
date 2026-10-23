//! `zapline snap`: save the traffic the running app recorded (in its dev
//! console) as snapshot tests in `tests/snapshots/`.
//!
//! The app does the writing: this command asks its dev console to save, over
//! a plain HTTP/1.1 request to the loopback address it listens on.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::ui;

/// Where the dev console saves recordings.
const SAVE_PATH: &str = "/_dev/recordings/save";

/// A `http://host:port` the app listens on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Host name or IP, without brackets.
    pub host: String,
    /// TCP port.
    pub port: u16,
}

impl Target {
    /// The `Host` header value.
    fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Parses `http://host[:port][/...]`. A host bound to every interface
/// (`0.0.0.0`, `[::]`) is reached on loopback.
pub fn parse_url(url: &str) -> Result<Target> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        anyhow!("`{url}` is not an http:// URL (the dev console is served over plain HTTP)")
    })?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6
            .split_once(']')
            .ok_or_else(|| anyhow!("`{url}`: unclosed `[` in the host"))?;
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        bail!("`{url}` has no host");
    }
    let port = match port {
        Some(p) => p
            .parse()
            .with_context(|| format!("`{url}`: invalid port `{p}`"))?,
        None => 80,
    };
    let host = match host {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        h => h,
    };
    Ok(Target {
        host: host.to_owned(),
        port,
    })
}

/// The URL the app's `main` listens on by default, read from the same
/// variables: `ADDR` (`0.0.0.0:3000`), else `PORT`, else 8080.
pub fn default_url(env: impl Fn(&str) -> Option<String>) -> String {
    if let Some(addr) = env("ADDR") {
        if let Ok(a) = addr.parse::<SocketAddr>() {
            return match a {
                SocketAddr::V4(v4) if v4.ip().is_unspecified() => {
                    format!("http://127.0.0.1:{}", a.port())
                }
                SocketAddr::V6(v6) if v6.ip().is_unspecified() => {
                    format!("http://[::1]:{}", a.port())
                }
                SocketAddr::V4(_) => format!("http://{a}"),
                SocketAddr::V6(_) => format!("http://{a}"),
            };
        }
    }
    let port = env("PORT")
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    format!("http://127.0.0.1:{port}")
}

/// Whether `name` can be a snapshot file name (the app checks it too).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// The HTTP/1.1 request that asks the console to save `name`.
pub fn save_request(target: &Target, name: &str) -> String {
    let body = format!("name={name}");
    format!(
        "POST {SAVE_PATH} HTTP/1.1\r\n\
         Host: {}\r\n\
         Accept: application/json\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         User-Agent: zapline-cli\r\n\
         \r\n\
         {body}",
        target.authority(),
        body.len()
    )
}

/// Status and body of a raw HTTP/1.1 response (`Content-Length`, chunked,
/// or read to the end).
pub fn parse_response(raw: &[u8]) -> Result<(u16, String)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("the app sent an incomplete HTTP response"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("the app sent an invalid HTTP status line"))?;
    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked") {
            chunked = true;
        } else if k.eq_ignore_ascii_case("content-length") {
            length = v.parse::<usize>().ok();
        }
    }
    let body = if chunked {
        dechunk(body)?
    } else {
        match length {
            Some(n) => body.get(..n).unwrap_or(body).to_vec(),
            None => body.to_vec(),
        }
    };
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow!("the app sent a truncated chunked body"))?;
        let size = std::str::from_utf8(&body[..eol])
            .ok()
            .map(|s| s.split(';').next().unwrap_or("").trim())
            .and_then(|s| usize::from_str_radix(s, 16).ok())
            .ok_or_else(|| anyhow!("the app sent an invalid chunk size"))?;
        body = &body[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        let chunk = body
            .get(..size)
            .ok_or_else(|| anyhow!("the app sent a truncated chunked body"))?;
        out.extend_from_slice(chunk);
        body = body.get(size + 2..).unwrap_or(&[]);
    }
}

fn send(target: &Target, request: &str, url: &str) -> Result<Vec<u8>> {
    let addrs: Vec<SocketAddr> = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve `{}`", target.host))?
        .collect();
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            Ok(mut stream) => {
                stream.set_read_timeout(Some(Duration::from_secs(30)))?;
                stream.write_all(request.as_bytes())?;
                let mut raw = Vec::new();
                stream
                    .read_to_end(&mut raw)
                    .context("the app did not answer")?;
                return Ok(raw);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(anyhow!(
        "cannot reach the app at {url}{}\n\
         hint: start it with `zapline dev --record`, or pass its address with --url",
        last.map(|e| format!(": {e}")).unwrap_or_default()
    ))
}

/// Asks the app at `url` to save its recording as `tests/snapshots/<name>.json`.
pub fn run(name: &str, url: &str) -> Result<()> {
    if !valid_name(name) {
        bail!("invalid snapshot name `{name}`: use 1 to 64 letters, digits, `-` or `_`");
    }
    let target = parse_url(url)?;
    let raw = send(&target, &save_request(&target, name), url)?;
    let (status, body) = parse_response(&raw)?;
    let json: Option<Value> = serde_json::from_str(&body).ok();
    match status {
        200 => {}
        404 => bail!(
            "no dev console at {url}\n\
             hint: snapshots are recorded by `.dev_console()` in a debug build of the app"
        ),
        _ => {
            let message = json
                .as_ref()
                .and_then(|j| j["error"].as_str())
                .map_or(body.clone(), str::to_owned);
            bail!("the app could not save the snapshot ({status}): {message}");
        }
    }
    let saved = json.ok_or_else(|| anyhow!("unexpected answer from the app: {body}"))?;
    let file = saved["file"].as_str().unwrap_or("tests/snapshots");
    let count = saved["exchanges"].as_u64().unwrap_or(0);
    ui::status("Saved", format!("{file} ({count} request(s))"));
    if let Some(harness) = saved["harness"].as_str() {
        ui::status(
            "Created",
            format!("{harness}, which replays every snapshot"),
        );
    }
    if let Some(note) = saved["note"].as_str() {
        ui::note(note);
    }
    ui::note(
        "run `cargo test --test snapshots`; accept intended changes with \
         ZAPLINE_UPDATE_SNAPSHOTS=1",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn urls() {
        assert_eq!(
            parse_url("http://localhost:3000/").unwrap(),
            Target {
                host: "localhost".into(),
                port: 3000
            }
        );
        assert_eq!(parse_url("http://0.0.0.0:8080").unwrap().host, "127.0.0.1");
        assert_eq!(parse_url("http://[::]:8080").unwrap().host, "::1");
        assert_eq!(parse_url("http://example.test").unwrap().port, 80);
        assert_eq!(
            parse_url("http://[::1]:9000").unwrap().authority(),
            "[::1]:9000"
        );
        assert!(parse_url("https://localhost").is_err());
        assert!(parse_url("http://:80").is_err());
        assert!(parse_url("http://x:port").is_err());
    }

    #[test]
    fn default_url_follows_the_app_main() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(default_url(env(&[])), "http://127.0.0.1:8080");
        assert_eq!(
            default_url(env(&[("PORT", "3000")])),
            "http://127.0.0.1:3000"
        );
        assert_eq!(
            default_url(env(&[("ADDR", "0.0.0.0:4000"), ("PORT", "3000")])),
            "http://127.0.0.1:4000"
        );
        assert_eq!(
            default_url(env(&[("ADDR", "127.0.0.2:5000")])),
            "http://127.0.0.2:5000"
        );
    }

    #[test]
    fn names() {
        assert!(valid_name("sign-up_2"));
        assert!(!valid_name(""));
        assert!(!valid_name("../x"));
        assert!(!valid_name(&"a".repeat(65)));
    }

    #[test]
    fn responses() {
        let (status, body) =
            parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}extra").unwrap();
        assert_eq!((status, body.as_str()), (200, "{}"));
        let (status, body) = parse_response(
            b"HTTP/1.1 409 Conflict\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!((status, body.as_str()), (409, "abcde"));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_err());
        assert!(parse_response(b"garbage\r\n\r\n").is_err());
    }

    /// A one-shot server standing in for the app.
    fn serve(answer: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let mut req = Vec::new();
            while !String::from_utf8_lossy(&req).contains("name=") {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            stream.write_all(answer.as_bytes()).unwrap();
            String::from_utf8_lossy(&req).into_owned()
        });
        (url, handle)
    }

    #[test]
    fn asks_the_app_to_save() {
        let body = r#"{"file":"tests/snapshots/signup.json","exchanges":3,"harness":"tests/snapshots.rs","note":null}"#;
        let answer: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (url, server) = serve(answer);
        run("signup", &url).unwrap();
        let request = server.join().unwrap();
        assert!(
            request.starts_with("POST /_dev/recordings/save HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(request.contains("Accept: application/json\r\n"));
        assert!(request.ends_with("\r\n\r\nname=signup"));
    }

    #[test]
    fn reports_what_the_app_says() {
        let (url, server) = serve(
            "HTTP/1.1 409 Conflict\r\ncontent-length: 52\r\n\r\n{\"error\":\"nothing recorded yet\",\"code\":\"x\"}        ",
        );
        let err = run("signup", &url).unwrap_err().to_string();
        server.join().unwrap();
        assert!(err.contains("(409): nothing recorded yet"), "{err}");

        let (url, server) = serve("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n");
        let err = run("signup", &url).unwrap_err().to_string();
        server.join().unwrap();
        assert!(err.contains("no dev console"), "{err}");

        assert!(run("../x", &url).is_err());
    }

    #[test]
    fn says_how_to_start_the_app_when_it_is_not_running() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = run("x", &format!("http://127.0.0.1:{port}"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot reach the app"), "{err}");
        assert!(err.contains("zapline dev --record"), "{err}");
    }
}
