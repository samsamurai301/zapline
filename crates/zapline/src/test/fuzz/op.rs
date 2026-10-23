//! Operations read from the OpenAPI document, the requests (cases) made
//! for them, and how a case becomes an HTTP request.

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::{Map, Value};

use super::gen::{apply, Gen, Mode, Seg};
use super::schema::Resolver;

/// Everything but RFC 3986 unreserved characters is percent-encoded.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Loc {
    Path,
    Query,
    Header,
    Cookie,
}

impl Loc {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Loc::Path => "path",
            Loc::Query => "query",
            Loc::Header => "header",
            Loc::Cookie => "cookie",
        }
    }
}

pub(crate) struct Param {
    pub(crate) name: String,
    pub(crate) loc: Loc,
    pub(crate) required: bool,
    pub(crate) schema: Value,
    /// A free-form query object (`HashMap<String, String>`): its entries
    /// are the query pairs.
    pub(crate) exploded_object: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BodyKind {
    Json,
    Form,
    Multipart,
    Text,
    Bytes,
}

impl BodyKind {
    fn of(content_type: &str) -> BodyKind {
        let essence = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if essence == "application/json" || essence.ends_with("+json") {
            BodyKind::Json
        } else if essence == "application/x-www-form-urlencoded" {
            BodyKind::Form
        } else if essence == "multipart/form-data" {
            BodyKind::Multipart
        } else if essence.starts_with("text/") {
            BodyKind::Text
        } else {
            BodyKind::Bytes
        }
    }
}

pub(crate) struct Media {
    pub(crate) content_type: String,
    pub(crate) kind: BodyKind,
    pub(crate) schema: Value,
}

pub(crate) struct Operation {
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) id: String,
    pub(crate) params: Vec<Param>,
    pub(crate) body: Vec<Media>,
    pub(crate) body_required: bool,
    pub(crate) responses: Map<String, Value>,
    /// Why the operation cannot be fuzzed (e.g. it upgrades to WebSocket).
    pub(crate) skip: Option<&'static str>,
}

impl Operation {
    /// `GET /users/{id}`.
    pub(crate) fn label(&self) -> String {
        format!("{} {}", self.method, self.path)
    }

    /// The response object documented for `status`: its code, its `NXX`
    /// range, or `default`.
    pub(crate) fn response(&self, status: u16) -> Option<&Value> {
        let range = format!("{}XX", status / 100);
        self.responses
            .get(&status.to_string())
            .or_else(|| self.responses.get(&range))
            .or_else(|| self.responses.get(&range.to_ascii_lowercase()))
            .or_else(|| self.responses.get("default"))
    }
}

/// The operations of an OpenAPI document, in document order (paths sorted).
pub(crate) fn operations(doc: &Value) -> Vec<Operation> {
    let r = Resolver::new(doc);
    let mut out = Vec::new();
    let Some(paths) = doc.get("paths").and_then(Value::as_object) else {
        return out;
    };
    for (path, item) in paths {
        let item = r.resolve(item);
        let shared = item.get("parameters").and_then(Value::as_array);
        for (key, method) in [
            ("get", Method::GET),
            ("put", Method::PUT),
            ("post", Method::POST),
            ("delete", Method::DELETE),
            ("options", Method::OPTIONS),
            ("head", Method::HEAD),
            ("patch", Method::PATCH),
            ("trace", Method::TRACE),
        ] {
            let Some(op) = item.get(key).and_then(Value::as_object) else {
                continue;
            };
            let mut params: Vec<Param> = Vec::new();
            let own = op.get("parameters").and_then(Value::as_array);
            // Operation-level parameters override path-level ones.
            for p in own
                .into_iter()
                .flatten()
                .chain(shared.into_iter().flatten())
            {
                let p = r.resolve(p);
                let (Some(name), Some(loc)) = (
                    p.get("name").and_then(Value::as_str),
                    p.get("in").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let loc = match loc {
                    "path" => Loc::Path,
                    "query" => Loc::Query,
                    "header" => Loc::Header,
                    "cookie" => Loc::Cookie,
                    _ => continue,
                };
                if params.iter().any(|q| q.name == name && q.loc == loc) {
                    continue;
                }
                let schema = p.get("schema").cloned().unwrap_or(Value::Bool(true));
                let exploded_object = loc == Loc::Query
                    && p.get("explode") != Some(&Value::Bool(false))
                    && r.types(&schema) == ["object"];
                params.push(Param {
                    name: name.to_owned(),
                    loc,
                    required: loc == Loc::Path || p.get("required") == Some(&Value::Bool(true)),
                    schema,
                    exploded_object,
                });
            }
            let mut body = Vec::new();
            let mut body_required = false;
            if let Some(rb) = op.get("requestBody") {
                let rb = r.resolve(rb);
                body_required = rb.get("required") == Some(&Value::Bool(true));
                for (ct, media) in rb
                    .get("content")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    body.push(Media {
                        content_type: ct.clone(),
                        kind: BodyKind::of(ct),
                        schema: media.get("schema").cloned().unwrap_or(Value::Bool(true)),
                    });
                }
            }
            let responses = op
                .get("responses")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let skip = if responses.contains_key("101") {
                Some("upgrades the connection (WebSocket)")
            } else if responses.values().any(|res| {
                res.get("content")
                    .and_then(Value::as_object)
                    .is_some_and(|c| c.keys().any(|k| k.starts_with("text/event-stream")))
            }) {
                Some("streams server-sent events")
            } else {
                None
            };
            let id = op
                .get("operationId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            out.push(Operation {
                method,
                path: path.clone(),
                id,
                params,
                body,
                body_required,
                responses,
                skip,
            });
        }
    }
    out
}

/// A request body, before encoding.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Body {
    None,
    /// A structured body for `body[media]`: JSON, form fields, multipart
    /// fields or text.
    Value {
        media: usize,
        value: Value,
    },
    /// Bytes as given, e.g. malformed JSON.
    Raw {
        content_type: Option<String>,
        bytes: Bytes,
    },
}

/// One generated request.
#[derive(Clone, Debug)]
pub(crate) struct Case {
    /// One slot per [`Operation::params`]; `None` leaves it out.
    pub(crate) params: Vec<Option<Value>>,
    pub(crate) body: Body,
    /// What this case tries, for the report.
    pub(crate) label: String,
}

/// The case as sent.
pub(crate) struct Rendered {
    pub(crate) method: Method,
    pub(crate) uri: String,
    pub(crate) headers: Vec<(HeaderName, HeaderValue)>,
    pub(crate) body: Bytes,
}

/// The wire strings of a parameter value.
fn wire(v: &Value) -> Vec<String> {
    match v {
        Value::Array(items) => items.iter().map(scalar).collect(),
        v => vec![scalar(v)],
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        v => v.to_string(),
    }
}

fn encode(s: &str) -> String {
    utf8_percent_encode(s, COMPONENT).to_string()
}

/// `a=1&b=x+y` (form encoding: spaces as `+`).
fn form_pairs<'s>(pairs: impl Iterator<Item = (&'s str, String)>) -> String {
    let mut out = String::new();
    for (k, v) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(&encode(k).replace("%20", "+"));
        out.push('=');
        out.push_str(&encode(&v).replace("%20", "+"));
    }
    out
}

/// The `(key, value)` pairs a form or exploded query object sends.
fn object_pairs(v: &Value) -> Vec<(String, String)> {
    match v {
        Value::Object(m) => m
            .iter()
            .flat_map(|(k, v)| wire(v).into_iter().map(move |s| (k.clone(), s)))
            .collect(),
        _ => Vec::new(),
    }
}

impl Case {
    /// The HTTP request for this case; `None` when it cannot be expressed
    /// (a header value with control characters, for instance).
    pub(crate) fn render(
        &self,
        op: &Operation,
        defaults: &[(HeaderName, HeaderValue)],
    ) -> Option<Rendered> {
        let mut path = op.path.clone();
        let mut query: Vec<(String, String)> = Vec::new();
        let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
        let mut cookies: Vec<String> = Vec::new();
        for (p, v) in op.params.iter().zip(&self.params) {
            match (p.loc, v) {
                (Loc::Path, v) => {
                    let text = v.as_ref().map(|v| wire(v).join(",")).unwrap_or_default();
                    path = path.replace(&format!("{{{}}}", p.name), &encode(&text));
                }
                (_, None) => {}
                (Loc::Query, Some(v)) if p.exploded_object => query.extend(object_pairs(v)),
                (Loc::Query, Some(v)) => {
                    query.extend(wire(v).into_iter().map(|s| (p.name.clone(), s)));
                }
                (Loc::Header, Some(v)) => {
                    let name = HeaderName::from_bytes(p.name.as_bytes()).ok()?;
                    let value = HeaderValue::from_str(&wire(v).join(",")).ok()?;
                    headers.push((name, value));
                }
                (Loc::Cookie, Some(v)) => {
                    cookies.push(format!("{}={}", p.name, encode(&wire(v).join(","))));
                }
            }
        }
        let mut uri = path;
        if !query.is_empty() {
            uri.push('?');
            uri.push_str(&form_pairs(
                query.iter().map(|(k, v)| (k.as_str(), v.clone())),
            ));
        }
        if !cookies.is_empty() {
            headers.push((
                http::header::COOKIE,
                HeaderValue::from_str(&cookies.join("; ")).ok()?,
            ));
        }
        let (content_type, body) = match &self.body {
            Body::None => (None, Bytes::new()),
            Body::Raw {
                content_type,
                bytes,
            } => (content_type.clone(), bytes.clone()),
            Body::Value { media, value } => {
                let m = op.body.get(*media)?;
                match m.kind {
                    BodyKind::Json => (
                        Some(m.content_type.clone()),
                        Bytes::from(serde_json::to_vec(value).ok()?),
                    ),
                    BodyKind::Form => {
                        let pairs = object_pairs(value);
                        (
                            Some(m.content_type.clone()),
                            Bytes::from(form_pairs(
                                pairs.iter().map(|(k, v)| (k.as_str(), v.clone())),
                            )),
                        )
                    }
                    BodyKind::Multipart => {
                        let mut form = crate::test::MultipartForm::new();
                        for (k, v) in object_pairs(value) {
                            form = form.text(k, v);
                        }
                        let (ct, bytes) = form.encode();
                        (Some(ct), Bytes::from(bytes))
                    }
                    BodyKind::Text | BodyKind::Bytes => {
                        (Some(m.content_type.clone()), Bytes::from(scalar(value)))
                    }
                }
            }
        };
        let mut all: Vec<(HeaderName, HeaderValue)> = defaults
            .iter()
            .filter(|(n, _)| !headers.iter().any(|(h, _)| h == n))
            .cloned()
            .collect();
        all.extend(headers);
        if let Some(ct) = content_type {
            all.push((http::header::CONTENT_TYPE, HeaderValue::from_str(&ct).ok()?));
        }
        http::Uri::try_from(uri.as_str()).ok()?;
        Some(Rendered {
            method: op.method.clone(),
            uri,
            headers: all,
            body,
        })
    }

    /// Whether every part of the request satisfies the document, as the
    /// server would decode it.
    pub(crate) fn is_valid(&self, op: &Operation, r: &Resolver<'_>, body_limit: usize) -> bool {
        for (p, v) in op.params.iter().zip(&self.params) {
            let Some(v) = v else {
                if p.required {
                    return false;
                }
                continue;
            };
            let strings = wire(v);
            if p.loc == Loc::Path && strings.iter().all(String::is_empty) {
                // An empty segment cannot match the route.
                return false;
            }
            if p.loc == Loc::Header
                && strings
                    .iter()
                    .any(|s| !s.bytes().all(|b| (0x20..0x7f).contains(&b)) || s.trim() != s)
            {
                return false;
            }
            if p.exploded_object {
                if !r.is_valid(&p.schema, &coerce_object(r, &p.schema, v)) {
                    return false;
                }
                continue;
            }
            if !r.is_valid(&p.schema, &coerce_param(r, &p.schema, &strings)) {
                return false;
            }
        }
        match &self.body {
            Body::None => !op.body_required,
            Body::Raw { .. } => false,
            Body::Value { media, value } => {
                let Some(m) = op.body.get(*media) else {
                    return false;
                };
                match m.kind {
                    BodyKind::Json => {
                        serde_json::to_vec(value).map_or(0, |b| b.len()) <= body_limit
                            && r.is_valid(&m.schema, value)
                    }
                    BodyKind::Form => r.is_valid(&m.schema, &coerce_object(r, &m.schema, value)),
                    BodyKind::Multipart | BodyKind::Text | BodyKind::Bytes => {
                        scalar(value).len() <= body_limit
                    }
                }
            }
        }
    }
}

/// A parameter's wire strings typed as the deserializer would.
fn coerce_param(r: &Resolver<'_>, schema: &Value, strings: &[String]) -> Value {
    let resolved = r.resolve(schema);
    if r.types(resolved).iter().any(|t| t == "array") {
        let items = resolved.get("items").cloned().unwrap_or(Value::Bool(true));
        return Value::Array(strings.iter().map(|s| r.coerce(&items, s)).collect());
    }
    r.coerce(schema, strings.first().map_or("", String::as_str))
}

/// Form fields (all strings on the wire) typed by the object's schema.
fn coerce_object(r: &Resolver<'_>, schema: &Value, v: &Value) -> Value {
    let resolved = r.resolve(schema);
    let props = resolved.get("properties").and_then(Value::as_object);
    let extra = resolved.get("additionalProperties");
    let Value::Object(fields) = v else {
        return v.clone();
    };
    let mut out = Map::new();
    for (k, v) in fields {
        let sub = props
            .and_then(|p| p.get(k))
            .or(extra.filter(|e| e.is_object()))
            .cloned()
            .unwrap_or(Value::Bool(true));
        out.insert(k.clone(), coerce_param(r, &sub, &wire(v)));
    }
    Value::Object(out)
}

/// Builds the cases for one operation.
pub(crate) struct Planner<'a, 'g, 'd> {
    pub(crate) op: &'a Operation,
    pub(crate) gen: &'g mut Gen<'d>,
    pub(crate) body_limit: usize,
}

impl Planner<'_, '_, '_> {
    /// Valid values for every parameter (optional ones included when
    /// `all`), and a body for the first media type.
    fn base(&mut self, mode: Mode, all: bool) -> Case {
        let params = self
            .op
            .params
            .iter()
            .map(|p| {
                let include = p.required || all || self.gen.rng.bool();
                include.then(|| self.param_value(p, mode))
            })
            .collect();
        let body = match self.op.body.first() {
            Some(m) if self.op.body_required || all || self.gen.rng.bool() => Body::Value {
                media: 0,
                value: self.gen.value(&m.schema, mode),
            },
            _ => Body::None,
        };
        Case {
            params,
            body,
            label: "valid request".into(),
        }
    }

    fn param_value(&mut self, p: &Param, mode: Mode) -> Value {
        for _ in 0..8 {
            let v = self.gen.value(&p.schema, mode);
            // Path segments must not be empty.
            if p.loc != Loc::Path || !scalar(&v).is_empty() {
                return v;
            }
        }
        Value::String("a".into())
    }

    /// The deterministic part of the plan: one typical request, then every
    /// boundary value and every schema violation, one location at a time.
    pub(crate) fn targeted(&mut self) -> Vec<Case> {
        let mut out = vec![self.base(Mode::Typical, false)];
        let op = self.op;

        for (i, p) in op.params.iter().enumerate() {
            let what = format!("{} parameter `{}`", p.loc.as_str(), p.name);
            let mut variants: Vec<(Option<Value>, String)> = Vec::new();
            for (v, label) in self.gen.edges(&p.schema) {
                variants.push((Some(v), format!("{label} for {what}")));
            }
            for (v, label) in self.gen.invalids(&p.schema) {
                variants.push((Some(v), format!("{label} for {what}")));
            }
            if p.loc != Loc::Path {
                let label = if p.required {
                    format!("missing required {what}")
                } else {
                    format!("without optional {what}")
                };
                variants.push((None, label));
            }
            let long = "x".repeat(8 * 1024);
            variants.push((
                Some(Value::String(long)),
                format!("8 KiB string for {what}"),
            ));
            for (v, label) in variants {
                let mut case = self.base(Mode::Full, true);
                case.params[i] = v;
                case.label = label;
                out.push(case);
            }
        }

        if let Some(media) = op.body.first() {
            let schema = media.schema.clone();
            let base = self.base(Mode::Full, true);
            let full = match &base.body {
                Body::Value { value, .. } => value.clone(),
                _ => Value::Null,
            };
            let structured = matches!(media.kind, BodyKind::Json | BodyKind::Form);
            let targets = if structured {
                self.gen.targets(&schema, &full)
            } else {
                Vec::new()
            };
            for t in &targets {
                let at = if t.at.is_empty() {
                    "the body".to_owned()
                } else {
                    format!("body field `{}`", t.pointer())
                };
                let mut variants: Vec<(Option<Value>, String)> = Vec::new();
                for (v, label) in self.gen.edges(&t.schema) {
                    variants.push((Some(v), format!("{label} for {at}")));
                }
                for (v, label) in self.gen.invalids(&t.schema) {
                    variants.push((Some(v), format!("{label} for {at}")));
                }
                if let Some(required) = t.property {
                    let label = if required {
                        format!("missing required {at}")
                    } else {
                        format!("without optional {at}")
                    };
                    variants.push((None, label));
                }
                for (v, label) in variants {
                    let mut value = full.clone();
                    apply(&mut value, &t.at, v);
                    let mut case = base.clone();
                    case.body = Body::Value { media: 0, value };
                    case.label = label;
                    out.push(case);
                }
            }

            let ct = media.content_type.clone();
            let mut raw = |bytes: Bytes, content_type: Option<String>, label: &str| {
                let mut case = base.clone();
                case.body = Body::Raw {
                    content_type,
                    bytes,
                };
                case.label = label.to_owned();
                out.push(case);
            };
            if media.kind == BodyKind::Json {
                raw(
                    Bytes::from_static(b"{\"unterminated\": "),
                    Some(ct.clone()),
                    "malformed JSON body",
                );
                raw(Bytes::new(), Some(ct.clone()), "empty JSON body");
                raw(
                    Bytes::from(serde_json::to_vec(&full).unwrap_or_default()),
                    Some("text/plain".into()),
                    "JSON body sent as text/plain",
                );
                let deep = format!("{}{}", "[".repeat(4096), "]".repeat(4096));
                raw(Bytes::from(deep), Some(ct.clone()), "deeply nested JSON");
            } else if media.kind == BodyKind::Text {
                raw(
                    Bytes::from_static(&[0xff, 0xfe, 0x00, 0xc3]),
                    Some(ct.clone()),
                    "body that is not UTF-8",
                );
            } else if media.kind == BodyKind::Multipart {
                raw(
                    Bytes::from_static(b"--x\r\n"),
                    Some("multipart/form-data".into()),
                    "multipart body without a boundary",
                );
            }
            raw(Bytes::new(), None, "no body and no content type");
            if self.body_limit < 16 * 1024 * 1024 {
                let n = self.body_limit + 1;
                let big = if media.kind == BodyKind::Json {
                    let mut b = Vec::with_capacity(n + 2);
                    b.push(b'"');
                    b.resize(n + 1, b'a');
                    b.push(b'"');
                    b
                } else {
                    vec![b'a'; n]
                };
                raw(Bytes::from(big), Some(ct), "body over the size limit");
            }
            if !op.body_required {
                let mut case = base.clone();
                case.body = Body::None;
                case.label = "without the optional body".into();
                out.push(case);
            }
        }
        out
    }

    /// A random request: random optional parts, a third of the scalars at
    /// their boundaries.
    pub(crate) fn random(&mut self) -> Case {
        let mut case = self.base(Mode::Random, false);
        case.label = "random request".into();
        case
    }
}

/// Simpler versions of `case`, for shrinking a failure.
pub(crate) fn simplifications(op: &Operation, case: &Case) -> Vec<Case> {
    let mut out = Vec::new();
    for (i, p) in op.params.iter().enumerate() {
        let Some(v) = &case.params[i] else {
            continue;
        };
        if p.loc != Loc::Path {
            let mut c = case.clone();
            c.params[i] = None;
            out.push(c);
        }
        for s in simpler(v, 0) {
            let mut c = case.clone();
            c.params[i] = Some(s);
            out.push(c);
        }
    }
    match &case.body {
        Body::Value { media, value } => {
            let mut c = case.clone();
            c.body = Body::None;
            out.push(c);
            for s in simpler(value, 0) {
                let mut c = case.clone();
                c.body = Body::Value {
                    media: *media,
                    value: s,
                };
                out.push(c);
            }
        }
        Body::Raw {
            content_type,
            bytes,
        } if bytes.len() > 1 && bytes.len() < 64 * 1024 => {
            let mut c = case.clone();
            c.body = Body::Raw {
                content_type: content_type.clone(),
                bytes: bytes.slice(..bytes.len() / 2),
            };
            out.push(c);
        }
        _ => {}
    }
    out
}

fn simpler(v: &Value, depth: usize) -> Vec<Value> {
    let mut out = Vec::new();
    if depth > 4 {
        return out;
    }
    match v {
        Value::String(s) if !s.is_empty() => {
            let n = s.chars().count();
            if n > 1 {
                out.push(Value::String(s.chars().take(1).collect()));
                out.push(Value::String(s.chars().take(n / 2).collect()));
                out.push(Value::String(s.chars().take(n - n / 4).collect()));
            }
            if s != "a" {
                out.push(Value::String("a".into()));
            }
        }
        Value::Array(items) if !items.is_empty() => {
            out.push(Value::Array(Vec::new()));
            if items.len() > 1 {
                out.push(Value::Array(items[..items.len() / 2].to_vec()));
            }
            for (i, item) in items.iter().enumerate().take(4) {
                for s in simpler(item, depth + 1) {
                    let mut c = items.clone();
                    c[i] = s;
                    out.push(Value::Array(c));
                }
            }
        }
        Value::Object(m) => {
            for k in m.keys() {
                let mut c = v.clone();
                apply(&mut c, &[Seg::Key(k.clone())], None);
                out.push(c);
            }
            for (k, item) in m {
                for s in simpler(item, depth + 1) {
                    let mut c = m.clone();
                    c.insert(k.clone(), s);
                    out.push(Value::Object(c));
                }
            }
        }
        Value::Number(n) if n.as_f64() != Some(0.0) => {
            if let Some(f) = n.as_f64() {
                if f.fract() != 0.0 && f.is_finite() && f.abs() < 1e15 {
                    out.push(serde_json::json!(f.trunc() as i64));
                }
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> Value {
        json!({ "paths": { "/items/{id}": { "get": {
            "operationId": "get_item",
            "parameters": [
                { "name": "id", "in": "path", "required": true, "schema": { "type": "integer", "format": "uint32", "minimum": 0 } },
                { "name": "q", "in": "query", "required": true, "schema": { "type": "string" } },
                { "name": "tags", "in": "query", "required": false, "schema": { "type": "array", "items": { "type": "integer" } } },
                { "name": "x-trace", "in": "header", "required": false, "schema": { "type": "string" } }
            ],
            "responses": { "200": { "description": "OK" }, "4XX": { "description": "Client error" } }
        } } } })
    }

    #[test]
    fn renders_and_classifies() {
        let doc = spec();
        let ops = operations(&doc);
        assert_eq!(ops.len(), 1);
        let op = &ops[0];
        assert_eq!(op.label(), "GET /items/{id}");
        assert!(op.response(404).is_some() && op.response(500).is_none());
        let r = Resolver::new(&doc);

        let case = Case {
            params: vec![
                Some(json!(7)),
                Some(json!("a b&c")),
                Some(json!([1, 2])),
                Some(json!("t1")),
            ],
            body: Body::None,
            label: String::new(),
        };
        let req = case.render(op, &[]).unwrap();
        assert_eq!(req.uri, "/items/7?q=a+b%26c&tags=1&tags=2");
        assert_eq!(req.headers[0].0, "x-trace");
        assert!(case.is_valid(op, &r, 1024));

        let mut bad = case.clone();
        bad.params[0] = Some(json!(-1));
        assert!(!bad.is_valid(op, &r, 1024));
        bad.params[0] = Some(json!("日本"));
        assert_eq!(
            bad.render(op, &[]).unwrap().uri.split('?').next(),
            Some("/items/%E6%97%A5%E6%9C%AC")
        );
        let mut missing = case.clone();
        missing.params[1] = None;
        assert!(!missing.is_valid(op, &r, 1024));
        let mut header = case.clone();
        header.params[3] = Some(json!("line\nbreak"));
        assert!(header.render(op, &[]).is_none());
    }

    #[test]
    fn plans_cover_every_parameter() {
        let doc = spec();
        let ops = operations(&doc);
        let mut gen = Gen::new(Resolver::new(&doc), 9);
        let cases = Planner {
            op: &ops[0],
            gen: &mut gen,
            body_limit: 1024,
        }
        .targeted();
        let has = |needle: &str| cases.iter().any(|c| c.label.contains(needle));
        assert!(has("boundary integer for path parameter `id`"));
        assert!(has("missing required query parameter `q`"));
        assert!(has("wrong type: a string for path parameter `id`"));
        assert!(cases.iter().any(|c| c.params[0] == Some(json!(0))));
    }
}
