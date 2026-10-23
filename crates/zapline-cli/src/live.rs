//! Tools that talk to the running dev app, for `zapline mcp`: its recent
//! requests and errors (from the dev console's JSON at `/_dev/api`), a
//! request sent to one of its routes, and the last test results.
//!
//! The app is found at, in order: the `url` the tool was given,
//! `$ZAPLINE_URL`, the address `zapline dev` records in
//! `target/zapline-dev.addr`, `$ADDR` / `$PORT`, and `http://127.0.0.1:8080`.
//! Requests are plain HTTP/1.1 to this machine; the console answers only
//! loopback clients.
//!
//! Every answer is a few lines: one per request or error, bodies cut to a
//! budget, only the headers worth reading.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::codes;

/// Where `zapline dev` records the app's URL, relative to the project root.
pub const ADDR_FILE: &str = "target/zapline-dev.addr";

/// The env var a debug build of the app reads to know where to record it.
pub const ENV_ADDR_FILE: &str = "ZAPLINE_DEV_ADDR_FILE";

/// Where the last `zapline test` / `check` results are kept, relative to the
/// project root: `target/zapline-last-<kind>.txt`.
pub fn last_results_file(root: &Path, kind: &str) -> PathBuf {
    root.join("target").join(format!("zapline-last-{kind}.txt"))
}

/// Keep a condensed result for `last_results` (best effort).
pub fn save_last_results(root: &Path, kind: &str, text: &str) {
    let path = last_results_file(root, kind);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}

/// Most body text `call_route` returns.
const BODY_BUDGET: usize = 2_000;

/// How long to wait for the app.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A response from the app.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// `200`.
    pub status: u16,
    /// `OK`.
    pub reason: String,
    /// Header names in lower case, in order.
    pub headers: Vec<(String, String)>,
    /// The decoded body (chunked transfer encoding undone).
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The first value of header `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// `http://host:port` and the prefix, from a URL (`https` is not spoken).
fn split_url(url: &str) -> Result<(String, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("only http:// URLs are supported, got `{url}`"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        bail!("no host in `{url}`");
    }
    // A server listening on every interface is reached on loopback.
    let authority = authority
        .replace("0.0.0.0", "127.0.0.1")
        .replace("[::]", "[::1]");
    let authority = if authority.contains(':') && !authority.ends_with(']') {
        authority
    } else {
        format!("{authority}:80")
    };
    Ok((authority, path.trim_end_matches('/').to_string()))
}

/// Sends one request and reads the whole response.
pub fn request(
    base: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<HttpResponse> {
    if !method.bytes().all(|b| b.is_ascii_alphabetic()) || method.is_empty() {
        bail!("invalid method `{method}`");
    }
    if !path.starts_with('/') || path.contains(['\r', '\n', ' ']) {
        bail!("the path must start with `/` and contain no spaces or line breaks: `{path}`");
    }
    let (authority, prefix) = split_url(base)?;
    let addr = authority
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve {authority}"))?
        .next()
        .ok_or_else(|| anyhow!("cannot resolve {authority}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| {
        anyhow!(
            "nothing answers at {base} ({e}) [ZL0091]: start the app with `zapline dev`, or pass `url`"
        )
    })?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let host = if addr.ip().is_loopback() {
        format!("localhost:{}", addr.port())
    } else {
        authority.clone()
    };
    let mut head = format!(
        "{} {prefix}{path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\nuser-agent: zapline-mcp\r\n",
        method.to_ascii_uppercase()
    );
    let has = |name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    if !has("accept") {
        // JSON, so the dev error page answers with problem+json.
        head.push_str("accept: application/json\r\n");
    }
    for (name, value) in headers {
        if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
            bail!("invalid header `{name}`");
        }
        if name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        if !has("content-type") {
            let json = serde_json::from_slice::<Value>(body).is_ok();
            head.push_str(if json {
                "content-type: application/json\r\n"
            } else {
                "content-type: text/plain; charset=utf-8\r\n"
            });
        }
        head.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(body.unwrap_or_default()))
        .context("cannot send the request")?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .context("cannot read the response")?;
    parse_response(&raw)
}

/// Parses an HTTP/1.1 response read to the end of the connection.
pub fn parse_response(raw: &[u8]) -> Result<HttpResponse> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("the app sent an incomplete response"))?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next();
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("bad status line `{status_line}`"))?;
    let reason = parts.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let mut body = raw[end + 4..].to_vec();
    let chunked = headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    if chunked {
        body = dechunk(&body)?;
    } else if let Some(len) = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
    {
        body.truncate(len);
    }
    Ok(HttpResponse {
        status,
        reason,
        headers,
        body,
    })
}

fn dechunk(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow!("bad chunked body"))?;
        let size_text = String::from_utf8_lossy(&data[..line_end]);
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| anyhow!("bad chunk size `{size_text}`"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size {
            bail!("truncated chunked body");
        }
        out.extend_from_slice(&data[..size]);
        data = data.get(size + 2..).unwrap_or_default();
    }
}

// ---------------------------------------------------------------------------
// Finding the app

/// The running app's base URL (see the module docs for the order).
pub fn app_url(given: Option<&str>, root: Option<&Path>) -> String {
    if let Some(url) = given.map(str::trim).filter(|u| !u.is_empty()) {
        return url.trim_end_matches('/').to_string();
    }
    if let Some(url) = std::env::var("ZAPLINE_URL").ok().filter(|u| !u.is_empty()) {
        return url.trim_end_matches('/').to_string();
    }
    if let Some(root) = root {
        if let Ok(text) = std::fs::read_to_string(root.join(ADDR_FILE)) {
            let url = text.trim();
            if !url.is_empty() {
                return url.trim_end_matches('/').to_string();
            }
        }
    }
    if let Ok(addr) = std::env::var("ADDR") {
        if !addr.is_empty() {
            return format!("http://{addr}");
        }
    }
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    format!("http://127.0.0.1:{port}")
}

/// The dev console's JSON.
fn console(base: &str) -> Result<Value> {
    let res = request(base, "GET", "/_dev/api", &[], None)?;
    if res.status == 404 {
        bail!(
            "{base} has no dev console [ZL0090]: add `.dev_console()` to the app (debug builds only)"
        );
    }
    if res.status != 200 {
        bail!("{base}/_dev/api answered {} {}", res.status, res.reason);
    }
    serde_json::from_slice(&res.body).context("the dev console sent invalid JSON")
}

// ---------------------------------------------------------------------------
// Tools

/// `recent_requests`: `500 GET /orders 0.8ms ZL0002`, newest first.
pub fn recent_requests(base: &str, limit: usize) -> Result<String> {
    let data = console(base)?;
    let requests = data["requests"].as_array().cloned().unwrap_or_default();
    if requests.is_empty() {
        return Ok("no requests yet".to_string());
    }
    let mut out = String::new();
    for r in requests.iter().take(limit) {
        let micros = r["micros"].as_u64().unwrap_or(0);
        out.push_str(&format!(
            "{} {} {} {}",
            r["status"],
            r["method"].as_str().unwrap_or("?"),
            r["path"].as_str().unwrap_or("?"),
            duration(micros)
        ));
        if let Some(code) = r["code"].as_str() {
            out.push(' ');
            out.push_str(code);
        }
        out.push('\n');
    }
    if requests.len() > limit {
        out.push_str(&format!("(+{} older)\n", requests.len() - limit));
    }
    Ok(out)
}

/// `recent_errors`: each with its code, message and fix, newest first.
pub fn recent_errors(base: &str, limit: usize) -> Result<String> {
    let data = console(base)?;
    let errors = data["errors"].as_array().cloned().unwrap_or_default();
    if errors.is_empty() {
        return Ok("no errors since the app started".to_string());
    }
    let mut out = String::new();
    for e in errors.iter().take(limit) {
        out.push_str(&format!(
            "{} {} {} {}: {}\n",
            e["code"].as_str().unwrap_or("-"),
            e["status"],
            e["method"].as_str().unwrap_or("?"),
            e["path"].as_str().unwrap_or("?"),
            e["message"].as_str().unwrap_or("")
        ));
        if let Some(at) = e["location"].as_str() {
            out.push_str(&format!("  at: {at}\n"));
        }
        if let Some(fix) = e["fix"].as_str() {
            out.push_str(&format!("  fix: {fix}\n"));
        }
    }
    if errors.len() > limit {
        out.push_str(&format!("(+{} older)\n", errors.len() - limit));
    }
    Ok(out)
}

/// Headers worth showing an assistant; the rest are noise.
const SHOWN_HEADERS: &[&str] = &[
    "content-type",
    "location",
    "allow",
    "retry-after",
    "www-authenticate",
    "x-zapline-diagnostic",
    "hx-redirect",
    "hx-trigger",
];

/// `call_route`: status line, the interesting headers, and the body (JSON
/// compacted, cut to a budget). A diagnostic code gets its fix appended.
pub fn call_route(
    base: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Result<String> {
    let res = request(base, method, path, headers, body.map(str::as_bytes))?;
    let mut out = format!("{} {}\n", res.status, res.reason);
    for (name, value) in &res.headers {
        if SHOWN_HEADERS.contains(&name.as_str()) {
            out.push_str(&format!("{name}: {value}\n"));
        } else if name == "set-cookie" {
            // The cookie's name and attributes; never its value.
            let (cookie, attrs) = value.split_once(';').unwrap_or((value, ""));
            let cookie_name = cookie.split('=').next().unwrap_or("").trim();
            out.push_str(&format!("set-cookie: {cookie_name}=…;{attrs}\n"));
        }
    }
    let text = String::from_utf8_lossy(&res.body);
    let text = match serde_json::from_str::<Value>(&text) {
        Ok(v) => v.to_string(),
        Err(_) => text.trim().to_string(),
    };
    if !text.is_empty() {
        out.push('\n');
        out.push_str(&cut(&text, BODY_BUDGET));
        out.push('\n');
    }
    // problem+json already carries `fix`; a bare header code does not.
    if let Some(code) = res.header("x-zapline-diagnostic").and_then(codes::lookup) {
        if !text.contains("\"fix\"") {
            out.push_str(&format!(
                "{}: {}\n  fix: {}\n",
                code.code,
                code.title,
                code.fix.replace("{T}", "T")
            ));
        }
    }
    Ok(out)
}

/// `explain_error`.
pub fn explain_error(code: &str) -> Result<String> {
    codes::explain(code)
}

/// `last_results`: the last `test` and `check` (and `fuzz`, when a tool
/// writes `target/zapline-last-fuzz.txt`) results, with their age.
pub fn last_results(root: &Path) -> String {
    let mut out = String::new();
    for kind in ["test", "check", "fuzz"] {
        let path = last_results_file(root, kind);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let age = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map_or_else(|| "?".to_string(), ago);
        out.push_str(&format!("## {kind} ({age} ago)\n{}\n", text.trim_end()));
    }
    if out.is_empty() {
        out.push_str("no results yet: run the `test` or `check` tool\n");
    }
    out
}

fn ago(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

fn duration(micros: u64) -> String {
    if micros < 1000 {
        format!("{micros}µs")
    } else {
        format!("{:.1}ms", micros as f64 / 1000.0)
    }
}

/// `text` cut to `budget` bytes on a character boundary.
fn cut(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let end = (0..=budget)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    format!("{}… [{} more bytes]", &text[..end], text.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_authority_and_prefix() {
        assert_eq!(
            split_url("http://0.0.0.0:8080").unwrap(),
            ("127.0.0.1:8080".into(), String::new())
        );
        assert_eq!(
            split_url("http://localhost/api/").unwrap(),
            ("localhost:80".into(), "/api".into())
        );
        assert_eq!(
            split_url("http://[::]:9000").unwrap(),
            ("[::1]:9000".into(), String::new())
        );
        assert!(split_url("https://x").is_err());
        assert!(split_url("http://").is_err());
    }

    #[test]
    fn parses_plain_and_chunked_responses() {
        let r = parse_response(
            b"HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nokEXTRA",
        )
        .unwrap();
        assert_eq!((r.status, r.reason.as_str()), (201, "Created"));
        assert_eq!(r.header("content-type"), Some("text/plain"));
        assert_eq!(r.body, b"ok");
        let r = parse_response(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\nzap\n\r\n3;x=y\r\nlin\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(r.body, b"zap\nlin");
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_err());
    }

    #[test]
    fn requests_reject_header_injection() {
        let bad = [("x\r\nevil".to_string(), "1".to_string())];
        assert!(request("http://127.0.0.1:1", "GET", "/", &bad, None).is_err());
        assert!(request("http://127.0.0.1:1", "GET", "/a b", &[], None).is_err());
        assert!(request("http://127.0.0.1:1", "G T", "/", &[], None).is_err());
    }

    #[test]
    fn a_missing_app_is_reported_with_its_code() {
        // Port 1 on loopback: nothing listens there.
        let err = recent_requests("http://127.0.0.1:1", 5)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[ZL0091]"), "{err}");
    }

    #[test]
    fn the_url_comes_from_the_argument_then_the_addr_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(app_url(Some("http://x:1/"), Some(tmp.path())), "http://x:1");
        std::fs::create_dir_all(tmp.path().join("target")).unwrap();
        std::fs::write(tmp.path().join(ADDR_FILE), "http://127.0.0.1:4321\n").unwrap();
        if std::env::var_os("ZAPLINE_URL").is_none() {
            assert_eq!(app_url(None, Some(tmp.path())), "http://127.0.0.1:4321");
        }
    }

    #[test]
    fn cut_keeps_characters_whole() {
        assert_eq!(cut("abc", 5), "abc");
        assert_eq!(cut("ééé", 3), "é… [4 more bytes]");
    }

    #[test]
    fn last_results_reads_what_was_saved() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(last_results(tmp.path()).starts_with("no results yet"));
        save_last_results(tmp.path(), "test", "ok: 3 passed, 0 failed, 0 ignored\n");
        let text = last_results(tmp.path());
        assert!(text.starts_with("## test (0s ago)\nok: 3 passed"), "{text}");
    }
}
