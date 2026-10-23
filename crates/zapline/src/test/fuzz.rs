//! OpenAPI-driven fuzzing: [`TestClient::fuzz`].

mod gen;
mod op;
mod schema;

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::future::{Future, IntoFuture};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Once};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::FutureExt;
use http::{HeaderName, HeaderValue, Method};
use serde_json::Value;

use self::gen::Gen;
use self::op::{operations, simplifications, Case, Operation, Planner, Rendered};
use self::schema::Resolver;
use super::{TestClient, TestResponse};

/// Env var: the seed when [`FuzzConfig::seed`] is not set.
pub const ENV_FUZZ_SEED: &str = "ZAPLINE_FUZZ_SEED";
/// Env var: the cases per operation when [`FuzzConfig::cases`] is not set.
pub const ENV_FUZZ_CASES: &str = "ZAPLINE_FUZZ_CASES";

const DEFAULT_SEED: u64 = 0x5eed;
const DEFAULT_CASES: usize = 100;
/// Requests spent shrinking each failure.
const SHRINK_BUDGET: usize = 200;

/// Statuses the framework answers invalid input with; accepted for
/// invalid requests even when an operation does not declare them.
const REJECTIONS: &[u16] = &[400, 404, 405, 411, 413, 414, 415, 422, 431];

/// Settings for [`TestClient::fuzz`].
///
/// ```
/// # use zapline::prelude::*;
/// use std::time::Duration;
/// use zapline::test::FuzzConfig;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// # let client = App::new().get("/", || async { "hi" }).test();
/// let config = FuzzConfig::new()
///     .seed(42)
///     .cases(300)
///     .bearer("test-token")
///     .timeout(Duration::from_secs(2))
///     .allow_status(409)
///     .skip("POST /webhooks");
/// client.fuzz().config(config).await.assert_clean();
/// # }
/// ```
#[derive(Clone, Debug, Default)]
pub struct FuzzConfig {
    seed: Option<u64>,
    cases: Option<usize>,
    headers: Vec<(HeaderName, HeaderValue)>,
    timeout: Option<Duration>,
    allow: Vec<u16>,
    only: Vec<String>,
    skip: Vec<String>,
    spec: Option<Arc<Value>>,
}

impl FuzzConfig {
    /// The defaults: seed `0x5eed` (or `ZAPLINE_FUZZ_SEED`), 100 cases per
    /// operation (or `ZAPLINE_FUZZ_CASES`), a 5 second timeout per request.
    pub fn new() -> Self {
        Self::default()
    }

    /// The seed: the same seed sends the same requests.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Requests per operation, before shrinking (at least 1).
    pub fn cases(mut self, cases: usize) -> Self {
        self.cases = Some(cases.max(1));
        self
    }

    /// A header sent with every request (credentials, a tenant id...).
    /// Parameters the document declares with the same name win.
    ///
    /// # Panics
    /// If the name or the value is not a valid header.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        let name = HeaderName::from_bytes(name.as_bytes()).expect("invalid fuzz header name");
        let value = HeaderValue::from_str(value).expect("invalid fuzz header value");
        self.headers.retain(|(n, _)| *n != name);
        self.headers.push((name, value));
        self
    }

    /// `Authorization: Bearer <token>` on every request.
    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", &format!("Bearer {token}"))
    }

    /// How long one request may take (default 5 seconds). Needs a Tokio
    /// runtime with timers enabled, as `#[tokio::test]` provides.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Accept `status` from every operation, documented or not (e.g. `429`
    /// from a rate limiter, or a `503` you expect).
    pub fn allow_status(mut self, status: u16) -> Self {
        self.allow.push(status);
        self
    }

    /// Fuzz only operations matching `pattern`: an operation id, or
    /// `METHOD /path` with the documented path (`GET /users/{id}`), or a
    /// path prefix (`/admin`). Repeatable.
    pub fn only(mut self, pattern: impl Into<String>) -> Self {
        self.only.push(pattern.into());
        self
    }

    /// Leave out operations matching `pattern` (same forms as
    /// [`only`](Self::only)). Repeatable.
    pub fn skip(mut self, pattern: impl Into<String>) -> Self {
        self.skip.push(pattern.into());
        self
    }

    /// Fuzz against this OpenAPI document instead of the app's own.
    pub fn spec(mut self, spec: Value) -> Self {
        self.spec = Some(Arc::new(spec));
        self
    }

    fn effective_seed(&self) -> u64 {
        self.seed
            .or_else(|| env_number(ENV_FUZZ_SEED))
            .unwrap_or(DEFAULT_SEED)
    }

    fn effective_cases(&self) -> usize {
        self.cases
            .or_else(|| env_number(ENV_FUZZ_CASES).map(|n| (n as usize).max(1)))
            .unwrap_or(DEFAULT_CASES)
    }

    fn selects(&self, op: &Operation) -> bool {
        let matches = |p: &String| {
            *p == op.id
                || p.eq_ignore_ascii_case(&op.label())
                || (p.starts_with('/') && op.path.starts_with(p.as_str()))
        };
        (self.only.is_empty() || self.only.iter().any(matches)) && !self.skip.iter().any(matches)
    }
}

/// `123` or `0x7b`.
fn env_number(name: &str) -> Option<u64> {
    let v = std::env::var(name).ok()?;
    let v = v.trim();
    match v.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => v.parse().ok(),
    }
}

impl TestClient {
    /// OpenAPI-driven fuzzing: every documented route, tested with requests
    /// generated from its own schemas.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// #[get("/items/{id}")]
    /// async fn item(Path(id): Path<u32>) -> String {
    ///     format!("item {id}")
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// App::new().mount(item).test().fuzz().await.assert_clean();
    /// # }
    /// ```
    ///
    /// It reads the app's OpenAPI document (the one its
    /// [`OpenApi`](crate::openapi::OpenApi) plugin serves, or a default one) and,
    /// for every operation, sends requests made from the JSON Schemas of its
    /// path, query, header and cookie parameters and its request body:
    ///
    /// - a **typical** valid request (schema examples first);
    /// - **boundary values** that are still valid: empty and maximal strings,
    ///   unicode (combining marks, emoji, right-to-left, NUL), `0`, `-1`, the
    ///   minimum and maximum of each integer type, empty and long arrays,
    ///   objects with only their required properties or with all of them;
    /// - **invalid input**: missing required parameters and fields, wrong
    ///   types, numbers out of range, strings of the wrong length, unknown enum
    ///   variants and properties, malformed and deeply nested JSON, the wrong
    ///   content type, a body over the size limit;
    /// - **random** requests, until the case budget is spent.
    ///
    /// A response is a failure when the handler **panics**, answers with a
    /// **5xx**, takes longer than the **timeout**, answers with a **status the
    /// document does not declare** for the operation, or sends a **2xx JSON body
    /// that does not match the declared response schema**. Invalid requests may
    /// also be rejected with the statuses the framework uses for bad input
    /// (`400`, `404`, `405`, `411`, `413`, `414`, `415`, `422`, `431`), even
    /// when the operation does not list them.
    ///
    /// Each failure is shrunk to a minimal reproducing request (optional parts
    /// removed, strings and arrays shortened) and reported once per operation
    /// and kind of failure. Runs are deterministic: the same seed sends the same
    /// requests. Set `ZAPLINE_FUZZ_SEED` or `ZAPLINE_FUZZ_CASES` to override
    /// the defaults without editing the test (`zapline fuzz --seed` does).
    ///
    /// An operation that times out gets no further cases, and a response
    /// that turns out to be an event stream (`text/event-stream`) is not
    /// read past its headers.
    ///
    /// The app shares state across cases, like a real server: requests that
    /// create things do create them. Each request comes from its own peer
    /// address, so per-IP rate limits don't hide routes.
    ///
    /// Await it for a [`FuzzReport`]; configure it with [`Fuzz::config`].
    pub fn fuzz(&self) -> Fuzz {
        Fuzz {
            client: self.clone(),
            config: FuzzConfig::new(),
        }
    }
}

/// A fuzz run, ready to be awaited. Made by [`TestClient::fuzz`].
#[must_use = "await it to run the fuzzer"]
pub struct Fuzz {
    client: TestClient,
    config: FuzzConfig,
}

impl Fuzz {
    /// Use these settings.
    pub fn config(mut self, config: FuzzConfig) -> Self {
        self.config = config;
        self
    }
}

impl IntoFuture for Fuzz {
    type Output = FuzzReport;
    type IntoFuture = Pin<Box<dyn Future<Output = FuzzReport> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(run(self.client, self.config))
    }
}

/// What went wrong, in a [`FuzzFailure`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailureKind {
    /// The handler (or a middleware) panicked; the message and location.
    Panic(String),
    /// A 5xx response.
    ServerError(u16),
    /// No response within the timeout.
    Timeout,
    /// A status the operation does not declare.
    UndeclaredStatus(u16),
    /// A 2xx body that does not match the declared response schema; one
    /// message per mismatch (`/items/0/id: expected integer, got string`).
    SchemaMismatch(Vec<String>),
}

impl FailureKind {
    /// Two failures are the same bug when they have the same kind, status
    /// and panic location.
    fn same_bug(&self, other: &FailureKind) -> bool {
        match (self, other) {
            (FailureKind::Panic(a), FailureKind::Panic(b)) => {
                panic_location(a) == panic_location(b)
            }
            (FailureKind::ServerError(a), FailureKind::ServerError(b))
            | (FailureKind::UndeclaredStatus(a), FailureKind::UndeclaredStatus(b)) => a == b,
            (FailureKind::Timeout, FailureKind::Timeout)
            | (FailureKind::SchemaMismatch(_), FailureKind::SchemaMismatch(_)) => true,
            _ => false,
        }
    }
}

fn panic_location(msg: &str) -> &str {
    msg.rsplit_once(" at ").map_or(msg, |(_, at)| at)
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FailureKind::Panic(msg) => write!(f, "panicked: {msg}"),
            FailureKind::ServerError(s) => write!(f, "server error {}", status_text(*s)),
            FailureKind::Timeout => f.write_str("timed out"),
            FailureKind::UndeclaredStatus(s) => write!(f, "undeclared status {}", status_text(*s)),
            FailureKind::SchemaMismatch(errors) => {
                write!(
                    f,
                    "response does not match its schema ({} error",
                    errors.len()
                )?;
                f.write_str(if errors.len() == 1 { ")" } else { "s)" })
            }
        }
    }
}

fn status_text(code: u16) -> String {
    match http::StatusCode::from_u16(code)
        .ok()
        .and_then(|s| s.canonical_reason())
    {
        Some(reason) => format!("{code} {reason}"),
        None => code.to_string(),
    }
}

/// A request the fuzzer sent, as it was sent.
#[derive(Clone, Debug)]
pub struct FuzzRequest {
    method: Method,
    uri: String,
    headers: Vec<(String, String)>,
    body: Bytes,
}

impl FuzzRequest {
    fn of(r: &Rendered, defaults: &[(HeaderName, HeaderValue)]) -> Self {
        FuzzRequest {
            method: r.method.clone(),
            uri: r.uri.clone(),
            // Configured headers (credentials) stay out of the report.
            headers: r
                .headers
                .iter()
                .filter(|(n, v)| !defaults.iter().any(|(dn, dv)| dn == n && dv == v))
                .map(|(n, v)| {
                    (
                        n.to_string(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect(),
            body: r.body.clone(),
        }
    }

    /// The HTTP method.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Path and query, percent-encoded.
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Headers the case set (not those from [`FuzzConfig::header`]).
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// The body as sent (empty for none).
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// The request as `TestClient` code, to paste into a regression test.
    ///
    /// ```text
    /// client.request(Method::POST, "/items").header("content-type", "application/json").body(r#"{"qty":0}"#).send().await
    /// ```
    pub fn to_rust(&self) -> String {
        let mut out = format!("client.request(Method::{}, {:?})", self.method, self.uri);
        for (n, v) in &self.headers {
            out.push_str(&format!(".header({n:?}, {v:?})"));
        }
        if !self.body.is_empty() {
            out.push_str(&format!(".body({})", rust_bytes(&self.body)));
        }
        out.push_str(".send().await");
        out
    }
}

/// A Rust expression for `body`.
fn rust_bytes(body: &[u8]) -> String {
    if let Some(&first) = body.first() {
        if body.len() > 64 && body.iter().all(|&b| b == first) {
            return format!("vec![{first}u8; {}]", body.len());
        }
    }
    match std::str::from_utf8(body) {
        Ok(text) if text.len() <= 4096 && !text.contains("\"#") => format!("r#\"{text}\"#"),
        Ok(text) if text.len() <= 4096 => format!("{text:?}"),
        _ if body.len() <= 256 => format!("vec!{body:?}"),
        _ => format!("/* {} bytes */ Vec::<u8>::new()", body.len()),
    }
}

impl fmt::Display for FuzzRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.method, self.uri)?;
        for (n, v) in &self.headers {
            write!(f, "\n{n}: {v}")?;
        }
        if !self.body.is_empty() {
            write!(f, "\n\n{}", snippet(&self.body, 512))?;
        }
        Ok(())
    }
}

/// `body` as text, cut to `max` bytes.
fn snippet(body: &[u8], max: usize) -> String {
    let cut = &body[..body.len().min(max)];
    let text = String::from_utf8_lossy(cut);
    if body.len() > max {
        format!("{text}... ({} bytes)", body.len())
    } else {
        text.into_owned()
    }
}

/// One bug found by the fuzzer, with the smallest request that shows it.
#[derive(Clone, Debug)]
pub struct FuzzFailure {
    operation: String,
    operation_id: String,
    kind: FailureKind,
    case: String,
    request: FuzzRequest,
    status: Option<u16>,
    response: Bytes,
    occurrences: usize,
}

impl FuzzFailure {
    /// The operation, as `METHOD /documented/{path}`.
    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// The operation's `operationId` (empty when the document has none).
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// What went wrong.
    pub fn kind(&self) -> &FailureKind {
        &self.kind
    }

    /// What the original failing case tried (e.g. "boundary integer for
    /// path parameter `id`").
    pub fn case(&self) -> &str {
        &self.case
    }

    /// The minimal reproducing request.
    pub fn request(&self) -> &FuzzRequest {
        &self.request
    }

    /// The response status (`None` after a panic or a timeout).
    pub fn status(&self) -> Option<u16> {
        self.status
    }

    /// The response body (empty after a panic or a timeout).
    pub fn response_body(&self) -> &Bytes {
        &self.response
    }

    /// How many requests hit this failure before shrinking.
    pub fn occurrences(&self) -> usize {
        self.occurrences
    }
}

/// The result of a fuzz run.
#[derive(Clone, Debug)]
pub struct FuzzReport {
    seed: u64,
    operations: usize,
    requests: usize,
    skipped: Vec<(String, &'static str)>,
    failures: Vec<FuzzFailure>,
}

impl FuzzReport {
    /// No failures.
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }

    /// One entry per operation and kind of failure, in document order.
    pub fn failures(&self) -> &[FuzzFailure] {
        &self.failures
    }

    /// Requests sent, shrinking included.
    pub fn requests(&self) -> usize {
        self.requests
    }

    /// Operations fuzzed.
    pub fn operations(&self) -> usize {
        self.operations
    }

    /// Operations left out, with the reason (WebSocket upgrades and event
    /// streams cannot be fuzzed request by request).
    pub fn skipped(&self) -> &[(String, &'static str)] {
        &self.skipped
    }

    /// The seed used: pass it to [`FuzzConfig::seed`] (or `ZAPLINE_FUZZ_SEED`)
    /// to send the same requests again.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Panic with the report unless no failure was found.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let client = App::new().get("/", || async { "hi" }).test();
    /// client.fuzz().await.assert_clean();
    /// # }
    /// ```
    #[track_caller]
    pub fn assert_clean(&self) -> &Self {
        if !self.is_clean() {
            panic!("{self}");
        }
        self
    }
}

impl fmt::Display for FuzzReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.failures.len();
        write!(
            f,
            "zapline fuzz: {} operation{}, {} requests, seed {}: ",
            self.operations,
            if self.operations == 1 { "" } else { "s" },
            self.requests,
            self.seed
        )?;
        match n {
            0 => f.write_str("no failures")?,
            1 => f.write_str("1 failure")?,
            n => write!(f, "{n} failures")?,
        }
        for (op, why) in &self.skipped {
            write!(f, "\n  skipped {op}: {why}")?;
        }
        for (i, fail) in self.failures.iter().enumerate() {
            write!(f, "\n\n{}) {}", i + 1, fail.operation)?;
            if !fail.operation_id.is_empty() {
                write!(f, " ({})", fail.operation_id)?;
            }
            write!(f, ": {}", fail.kind)?;
            if let FailureKind::SchemaMismatch(errors) = &fail.kind {
                for e in errors.iter().take(5) {
                    write!(f, "\n   - {e}")?;
                }
                if errors.len() > 5 {
                    write!(f, "\n   - ... {} more", errors.len() - 5)?;
                }
            }
            let times = match fail.occurrences {
                1 => String::new(),
                n => format!(" (hit by {n} requests)"),
            };
            write!(f, "\n   case: {}{times}", fail.case)?;
            let request = fail.request.to_string().replace('\n', "\n            ");
            write!(f, "\n   request: {request}")?;
            match fail.status {
                Some(s) => write!(f, "\n   response: {}", status_text(s))?,
                None => f.write_str("\n   response: none")?,
            }
            if !fail.response.is_empty() {
                write!(f, " {}", snippet(&fail.response, 300).replace('\n', " "))?;
            }
            write!(f, "\n   reproduce: {}", fail.request.to_rust())?;
        }
        if n > 0 {
            write!(
                f,
                "\n\nrerun the same requests with {ENV_FUZZ_SEED}={}",
                self.seed
            )?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The run

/// The OpenAPI document to fuzz: the configured one, the one the app's
/// `OpenApi` plugin serves, or one generated with default settings.
fn document(client: &TestClient, config: &FuzzConfig) -> Arc<Value> {
    if let Some(spec) = &config.spec {
        return spec.clone();
    }
    let served = client
        .routes()
        .iter()
        .find_map(|r| r.meta::<crate::openapi::ServedSpec>());
    match served {
        Some(spec) => spec.0.clone(),
        None => Arc::new(crate::openapi::OpenApi::new("app", "0").generate(client.routes())),
    }
}

/// A stable per-operation seed, so adding a route leaves the cases of the
/// others unchanged.
fn op_seed(seed: u64, op: &Operation) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in op.label().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h ^ seed.rotate_left(17)
}

#[derive(Clone)]
struct Outcome {
    status: Option<u16>,
    body: Bytes,
    failure: Option<FailureKind>,
}

async fn run(client: TestClient, config: FuzzConfig) -> FuzzReport {
    client.start().await;
    let body_limit = client.body_limit().await;
    let doc = document(&client, &config);
    let r = Resolver::new(&doc);
    let seed = config.effective_seed();
    let cases = config.effective_cases();
    let timeout = config.timeout.unwrap_or(Duration::from_secs(5));

    let mut report = FuzzReport {
        seed,
        operations: 0,
        requests: 0,
        skipped: Vec::new(),
        failures: Vec::new(),
    };
    for op in operations(&doc).iter().filter(|op| config.selects(op)) {
        if let Some(why) = op.skip {
            report.skipped.push((op.label(), why));
            continue;
        }
        report.operations += 1;
        let mut gen = Gen::new(r, op_seed(seed, op));
        let mut planner = Planner {
            op,
            gen: &mut gen,
            body_limit,
        };
        let mut plan = planner.targeted();
        // Keep the typical case first; sample the rest when over budget,
        // leaving a fifth of the budget for random requests.
        let targeted_budget = (cases - cases / 5).max(1);
        if plan.len() > targeted_budget {
            let mut rest = plan.split_off(1);
            planner.gen.rng.shuffle(&mut rest);
            rest.truncate(targeted_budget - 1);
            plan.extend(rest);
        }
        while plan.len() < cases {
            plan.push(planner.random());
        }

        let mut found: Vec<Found> = Vec::new();
        for case in plan {
            let Some(rendered) = case.render(op, &config.headers) else {
                continue;
            };
            report.requests += 1;
            let valid = case.is_valid(op, &r, body_limit);
            let outcome = execute(&client, op, &r, &rendered, valid, &config, timeout).await;
            let Some(kind) = outcome.failure.clone() else {
                continue;
            };
            let timed_out = kind == FailureKind::Timeout;
            match found.iter_mut().find(|f| f.kind.same_bug(&kind)) {
                Some(f) => f.occurrences += 1,
                None => found.push(Found {
                    kind,
                    case,
                    rendered,
                    outcome,
                    occurrences: 1,
                }),
            }
            // Each further timeout would cost the full timeout again.
            if timed_out {
                break;
            }
        }

        for f in found {
            let occurrences = f.occurrences;
            let (f, spent) = shrink(&client, op, &r, f, body_limit, &config, timeout).await;
            report.requests += spent;
            report.failures.push(FuzzFailure {
                operation: op.label(),
                operation_id: op.id.clone(),
                kind: f.kind,
                case: f.case.label,
                request: FuzzRequest::of(&f.rendered, &config.headers),
                status: f.outcome.status,
                response: f.outcome.body,
                occurrences,
            });
        }
    }
    report
}

/// A failing request: the first one seen for a bug, later the smallest.
struct Found {
    kind: FailureKind,
    case: Case,
    rendered: Rendered,
    outcome: Outcome,
    occurrences: usize,
}

/// Replace the failing request with simpler ones that still show the same
/// bug, within [`SHRINK_BUDGET`] requests. Returns the smallest one found,
/// with the response it got, and the requests spent.
async fn shrink(
    client: &TestClient,
    op: &Operation,
    r: &Resolver<'_>,
    mut found: Found,
    body_limit: usize,
    config: &FuzzConfig,
    timeout: Duration,
) -> (Found, usize) {
    let mut spent = 0;
    // Timeouts are slow to confirm: keep the original request.
    let mut budget = if found.kind == FailureKind::Timeout {
        0
    } else {
        SHRINK_BUDGET
    };
    'outer: while budget > 0 {
        for candidate in simplifications(op, &found.case) {
            if budget == 0 {
                break 'outer;
            }
            let Some(rendered) = candidate.render(op, &config.headers) else {
                continue;
            };
            budget -= 1;
            spent += 1;
            let valid = candidate.is_valid(op, r, body_limit);
            let outcome = execute(client, op, r, &rendered, valid, config, timeout).await;
            if let Some(kind) = outcome.failure.clone().filter(|k| k.same_bug(&found.kind)) {
                found.case = Case {
                    label: std::mem::take(&mut found.case.label),
                    ..candidate
                };
                found.kind = kind;
                found.rendered = rendered;
                found.outcome = outcome;
                continue 'outer;
            }
        }
        break;
    }
    (found, spent)
}

async fn execute(
    client: &TestClient,
    op: &Operation,
    r: &Resolver<'_>,
    rendered: &Rendered,
    valid: bool,
    config: &FuzzConfig,
    timeout: Duration,
) -> Outcome {
    let mut req = client
        .request(rendered.method.clone(), &rendered.uri)
        .remote_addr(next_peer());
    for (n, v) in &rendered.headers {
        req = req.header(n, v);
    }
    let req = req.body(rendered.body.clone());

    install_panic_hook();
    let slot: Arc<Mutex<Option<String>>> = Arc::default();
    let sent = Captured {
        inner: Box::pin(req.send_skipping_streams()),
        slot: slot.clone(),
    };
    let result = tokio::time::timeout(timeout, AssertUnwindSafe(sent).catch_unwind()).await;
    let panicked = slot.lock().ok().and_then(|mut s| s.take());
    let res: TestResponse = match result {
        Err(_) => {
            return Outcome {
                status: None,
                body: Bytes::new(),
                failure: Some(FailureKind::Timeout),
            }
        }
        Ok(Err(payload)) => {
            return Outcome {
                status: None,
                body: Bytes::new(),
                failure: Some(FailureKind::Panic(
                    panicked.unwrap_or_else(|| payload_message(&*payload)),
                )),
            }
        }
        Ok(Ok(res)) => res,
    };
    let status = res.status().as_u16();
    let content_type = res.header("content-type").map(str::to_owned);
    let body = res.bytes().clone();
    let failure = judge(
        op,
        r,
        status,
        content_type.as_deref(),
        &body,
        valid,
        config,
        panicked,
    );
    Outcome {
        status: Some(status),

        body,
        failure,
    }
}

/// A new peer address per request, from the benchmarking range
/// `198.18.0.0/15`, so per-IP rate limits don't turn every case after the
/// first hundred into a `429`.
fn next_peer() -> std::net::SocketAddr {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed) % (1 << 17);
    let ip = std::net::Ipv4Addr::from(u32::from(std::net::Ipv4Addr::new(198, 18, 0, 0)) + n);
    std::net::SocketAddr::from((ip, 40000))
}

/// Whether a response is a failure.
#[allow(clippy::too_many_arguments)]
fn judge(
    op: &Operation,
    r: &Resolver<'_>,
    status: u16,
    content_type: Option<&str>,
    body: &[u8],
    valid: bool,
    config: &FuzzConfig,
    panicked: Option<String>,
) -> Option<FailureKind> {
    if let Some(msg) = panicked {
        // A panic turned into a response (e.g. by `CatchPanic`).
        return Some(FailureKind::Panic(msg));
    }
    let allowed = config.allow.contains(&status);
    if status >= 500 && !allowed {
        return Some(FailureKind::ServerError(status));
    }
    let declared = op.response(status);
    if declared.is_none() && !allowed && !(!valid && REJECTIONS.contains(&status)) {
        return Some(FailureKind::UndeclaredStatus(status));
    }
    if !(200..300).contains(&status) {
        return None;
    }
    let content = declared?.get("content")?.as_object()?;
    if content.is_empty() {
        return None;
    }
    let essence = |ct: &str| {
        ct.split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
    };
    let actual = content_type.map(essence).unwrap_or_default();
    let media = content
        .iter()
        .find(|(k, _)| essence(k) == actual)
        .or_else(|| content.iter().find(|(k, _)| *k == "*/*"));
    let Some((_, media)) = media else {
        if body.is_empty() {
            return None;
        }
        let declared: Vec<&str> = content.keys().map(String::as_str).collect();
        return Some(FailureKind::SchemaMismatch(vec![format!(
            "content type {:?} is not one of the declared {}",
            content_type.unwrap_or("(none)"),
            declared.join(", ")
        )]));
    };
    let is_json = actual == "application/json" || actual.ends_with("+json");
    let schema = media.get("schema")?;
    if !is_json {
        return None;
    }
    let value: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return Some(FailureKind::SchemaMismatch(vec![format!(
                "body is not valid JSON: {e}"
            )]))
        }
    };
    let errors = r.validate(schema, &value);
    (!errors.is_empty()).then_some(FailureKind::SchemaMismatch(errors))
}

// ---------------------------------------------------------------------------
// Panics

thread_local! {
    /// Set while a fuzz request is being polled on this thread.
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// The last panic message (with location) seen while `ACTIVE`.
    static CAPTURED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Chains a panic hook that records panics raised while a fuzz request is
/// polled (and keeps them off stderr: they are in the report), leaving
/// every other panic to the previous hook.
fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if ACTIVE.with(Cell::get) {
                let msg = payload_message(info.payload());
                let msg = match info.location() {
                    Some(l) => format!("{msg} at {}:{}:{}", l.file(), l.line(), l.column()),
                    None => msg,
                };
                CAPTURED.with(|c| *c.borrow_mut() = Some(msg));
            } else {
                previous(info);
            }
        }));
    });
}

fn payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Marks this thread `ACTIVE` while the request is polled, wherever the
/// runtime polls it, and moves a panic recorded during the poll to `slot`
/// (also when the poll unwinds).
struct Captured<F> {
    inner: Pin<Box<F>>,
    slot: Arc<Mutex<Option<String>>>,
}

impl<F: Future> Future for Captured<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        struct Reset<'a>(&'a Mutex<Option<String>>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                ACTIVE.with(|a| a.set(false));
                if let Some(msg) = CAPTURED.with(|c| c.borrow_mut().take()) {
                    if let Ok(mut slot) = self.0.lock() {
                        *slot = Some(msg);
                    }
                }
            }
        }
        let this = &mut *self;
        ACTIVE.with(|a| a.set(true));
        let _reset = Reset(&this.slot);
        this.inner.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bugs_are_grouped_by_kind_and_location() {
        let a = FailureKind::Panic("boom at src/a.rs:1:1".into());
        let b = FailureKind::Panic("other message at src/a.rs:1:1".into());
        let c = FailureKind::Panic("boom at src/b.rs:9:1".into());
        assert!(a.same_bug(&b) && !a.same_bug(&c));
        assert!(FailureKind::ServerError(500).same_bug(&FailureKind::ServerError(500)));
        assert!(!FailureKind::ServerError(500).same_bug(&FailureKind::ServerError(502)));
    }

    #[test]
    fn requests_print_as_rust() {
        let r = FuzzRequest {
            method: Method::POST,
            uri: "/items?q=a+b".into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: Bytes::from_static(br#"{"qty":0}"#),
        };
        assert_eq!(
            r.to_rust(),
            r##"client.request(Method::POST, "/items?q=a+b").header("content-type", "application/json").body(r#"{"qty":0}"#).send().await"##
        );
        assert_eq!(rust_bytes(&[b'a'; 100]), "vec![97u8; 100]");
    }
}
