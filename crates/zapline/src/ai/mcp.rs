//! Serving [`Tools`] over the Model Context Protocol (Streamable HTTP,
//! JSON responses): `App::mcp("/mcp", tools)` lets MCP clients (Claude
//! Desktop, Claude Code, IDEs) list and call them.

use http::header::{CONTENT_TYPE, HOST, ORIGIN};
use http::{HeaderValue, StatusCode};
use serde_json::{json, Value};

use super::tools::{ToolContext, Tools};
use crate::request::Request;
use crate::response::{IntoResponse, Response};
use crate::router::Routes;
use crate::App;

/// Protocol versions this server speaks, newest first.
const VERSIONS: [&str; 2] = ["2025-06-18", "2025-03-26"];

impl App {
    /// Serve `tools` to MCP clients at `path` (`POST` for JSON-RPC
    /// messages). Requests from a web page on another origin are refused, as
    /// the MCP spec requires, and so are web pages calling it under a host
    /// other than `localhost`, `127.0.0.1`, `[::1]` or those added with
    /// [`Tools::mcp_allowed_hosts`]: a page could otherwise reach a server
    /// on `localhost` through DNS rebinding. To require a login, mount it
    /// with [`Routes::mcp`] in a group wrapped with your authentication.
    ///
    /// ```
    /// use zapline::ai::{ToolContext, Tools};
    /// use zapline::prelude::*;
    ///
    /// #[model]
    /// struct Add { a: i64, b: i64 }
    ///
    /// let tools = Tools::new()
    ///     .tool("add", "Add two numbers.", |x: Add, _cx: ToolContext| async move { Ok(x.a + x.b) });
    /// let app = App::new().mcp("/mcp", tools);
    /// ```
    pub fn mcp(self, path: &str, tools: Tools) -> Self {
        self.nest("/", Routes::new().mcp(path, tools))
    }
}

impl Routes {
    /// [`App::mcp`] for a route group, e.g. one wrapped with authentication.
    ///
    /// ```
    /// use zapline::ai::Tools;
    /// use zapline::prelude::*;
    ///
    /// let routes = Routes::new().mcp("/mcp", Tools::new());
    /// ```
    pub fn mcp(self, path: &str, tools: Tools) -> Self {
        self.post(path, move |req: Request| {
            let tools = tools.clone();
            async move { handle(req, &tools).await }
        })
        .get(path, || async {
            // No server-to-client event stream: everything is answered
            // in the POST response.
            StatusCode::METHOD_NOT_ALLOWED
        })
    }
}

/// Whether a browser page may make this request: no `Origin` (not a
/// browser), or an `Origin` that is this server under an allowed host.
/// Matching `Origin` to `Host` alone isn't enough: under DNS rebinding
/// both name the attacker's domain.
fn browser_allowed(req: &Request, tools: &Tools) -> bool {
    let Some(origin) = req.headers().get(ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    // HTTP/2 and HTTP/3 carry the host as the URI's authority.
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_owned()))
        .unwrap_or_default();
    let origin_host = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    !host.is_empty() && origin_host.eq_ignore_ascii_case(&host) && host_allowed(&host, tools)
}

/// `host` (with or without a port) is a loopback name or one the app allowed.
fn host_allowed(host: &str, tools: &Tools) -> bool {
    let host = host.to_ascii_lowercase();
    let name = if host.starts_with('[') {
        host.find(']').map_or(host.as_str(), |i| &host[..=i])
    } else {
        host.split_once(':').map_or(host.as_str(), |(name, _)| name)
    };
    matches!(name, "localhost" | "127.0.0.1" | "[::1]")
        || tools.mcp_hosts.iter().any(|h| *h == host || *h == name)
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    let mut res = (status, body.to_string()).into_response();
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res
}

async fn handle(mut req: Request, tools: &Tools) -> Response {
    if !browser_allowed(&req, tools) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let is_json = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    // After `initialize`, clients name the negotiated version on every
    // request; one this server doesn't speak is refused (absent: assumed).
    if let Some(v) = req.headers().get("mcp-protocol-version") {
        if !v.to_str().is_ok_and(|v| VERSIONS.contains(&v)) {
            return json_response(
                StatusCode::BAD_REQUEST,
                &rpc_error(Value::Null, -32600, "unsupported MCP-Protocol-Version"),
            );
        }
    }
    let cx = ToolContext::from_request(&req);
    let body = match req.body_bytes().await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &rpc_error(Value::Null, -32700, "parse error"),
        );
    };
    if !msg.is_object() || msg["jsonrpc"] != "2.0" {
        return json_response(
            StatusCode::BAD_REQUEST,
            &rpc_error(Value::Null, -32600, "expected one JSON-RPC 2.0 message"),
        );
    }
    let Some(id) = msg.get("id").cloned() else {
        // A notification (`notifications/initialized`, ...): nothing to answer.
        return StatusCode::ACCEPTED.into_response();
    };
    let params = &msg["params"];
    let reply = match msg["method"].as_str().unwrap_or("") {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = VERSIONS
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&VERSIONS[0]);
            json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "zapline", "version": env!("CARGO_PKG_VERSION")},
            }})
        }
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools.mcp_json()}}),
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return json_response(StatusCode::OK, &rpc_error(id, -32602, "missing tool name"));
            };
            if !tools.names().contains(&name) {
                return json_response(StatusCode::OK, &rpc_error(id, -32602, "unknown tool"));
            }
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let (text, is_error) = tools.call(name, args, &cx).await;
            json!({"jsonrpc": "2.0", "id": id, "result": {
                "content": [{"type": "text", "text": text}],
                "isError": is_error,
            }})
        }
        _ => rpc_error(id, -32601, "method not found"),
    };
    json_response(StatusCode::OK, &reply)
}
