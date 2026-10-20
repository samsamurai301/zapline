//! The HTTP request metrics behind the [`Metrics`](super::Metrics) plugin.
//!
//! Hot-path design:
//! - per-route series are pre-registered in `on_build` and found by the
//!   matched route's address (a tiny open-addressing table, no hashing of
//!   strings, no locks);
//! - every counter is striped across cache-line-aligned shards and each
//!   thread writes to its own shard, so worker threads never contend on a
//!   cache line;
//! - status codes get one of a few lazily claimed columns per series;
//! - all of it is leaked once per app (like the route table) so request
//!   futures hold plain `&'static` references instead of `Arc`s.

use std::cell::Cell;
use std::sync::atomic::{AtomicI64, AtomicU16, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use http::Method;

use super::registry::{encode_histogram, le_labels, Encoder, MetricKind, Num};
use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::router::RouteInfo;

// ---------------------------------------------------------------------------
// Sharding

/// Eight counters on their own cache line.
#[repr(align(64))]
struct Line([AtomicU64; 8]);

impl Line {
    fn new() -> Self {
        Line(std::array::from_fn(|_| AtomicU64::new(0)))
    }
}

#[repr(align(64))]
struct PaddedI64(AtomicI64);

fn shard_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        (cpus * 2).next_power_of_two().clamp(2, 32)
    })
}

/// This thread's shard. Threads are assigned round-robin on first use.
#[inline]
fn shard() -> usize {
    thread_local! {
        static SHARD: Cell<usize> = const { Cell::new(usize::MAX) };
    }
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    SHARD
        .try_with(|s| {
            let v = s.get();
            if v != usize::MAX {
                return v;
            }
            let v = NEXT.fetch_add(1, Relaxed) & (shard_count() - 1);
            s.set(v);
            v
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Series

const COLS: usize = 8;

/// Request counts by exact status code. Each series claims up to `COLS`
/// status columns on first use (lock-free); rarer extra codes go to a
/// mutex-protected overflow list.
struct StatusCounts {
    codes: [AtomicU16; COLS],
    cells: Box<[Line]>,
    overflow: Mutex<Vec<(u16, u64)>>,
}

impl StatusCounts {
    fn new() -> Self {
        StatusCounts {
            codes: std::array::from_fn(|_| AtomicU16::new(0)),
            cells: (0..shard_count()).map(|_| Line::new()).collect(),
            overflow: Mutex::new(Vec::new()),
        }
    }

    #[inline]
    fn record(&self, shard: usize, code: u16) {
        for (i, slot) in self.codes.iter().enumerate() {
            let mut c = slot.load(Relaxed);
            if c == 0 {
                c = match slot.compare_exchange(0, code, Relaxed, Relaxed) {
                    Ok(_) => code,
                    Err(actual) => actual,
                };
            }
            if c == code {
                self.cells[shard].0[i].fetch_add(1, Relaxed);
                return;
            }
        }
        let mut o = self.overflow.lock().unwrap_or_else(|e| e.into_inner());
        match o.iter_mut().find(|(c, _)| *c == code) {
            Some((_, n)) => *n += 1,
            None => o.push((code, 1)),
        }
    }

    /// Non-zero `(code, count)` pairs sorted by code.
    fn snapshot(&self) -> Vec<(u16, u64)> {
        let mut out: Vec<(u16, u64)> = Vec::new();
        for (i, slot) in self.codes.iter().enumerate() {
            let code = slot.load(Relaxed);
            if code == 0 {
                break;
            }
            let n: u64 = self.cells.iter().map(|l| l.0[i].load(Relaxed)).sum();
            if n > 0 {
                out.push((code, n));
            }
        }
        out.extend(
            self.overflow
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .copied(),
        );
        out.sort_unstable_by_key(|(c, _)| *c);
        out
    }
}

/// Latency histogram: per-bucket counts (last = `+Inf`) then the sum in
/// nanoseconds, `stride` lines per shard.
struct Latency {
    stride: usize,
    cells: Box<[Line]>,
}

impl Latency {
    fn new(buckets: usize) -> Self {
        let slots = buckets + 2;
        let stride = slots.div_ceil(8);
        Latency {
            stride,
            cells: (0..shard_count() * stride).map(|_| Line::new()).collect(),
        }
    }

    #[inline]
    fn slot(&self, shard: usize, i: usize) -> &AtomicU64 {
        &self.cells[shard * self.stride + i / 8].0[i % 8]
    }

    #[inline]
    fn observe(&self, bounds: &[u64], shard: usize, nanos: u64) {
        let i = bounds.partition_point(|&b| b < nanos);
        self.slot(shard, i).fetch_add(1, Relaxed);
        self.slot(shard, bounds.len() + 1).fetch_add(nanos, Relaxed);
    }

    /// Per-bucket counts (incl. `+Inf`) and the sum in seconds.
    fn snapshot(&self, buckets: usize) -> (Vec<u64>, f64) {
        let shards = self.cells.len() / self.stride;
        let mut counts = vec![0u64; buckets + 1];
        let mut sum = 0u64;
        for s in 0..shards {
            for (i, c) in counts.iter_mut().enumerate() {
                *c += self.slot(s, i).load(Relaxed);
            }
            sum = sum.wrapping_add(self.slot(s, buckets + 1).load(Relaxed));
        }
        (counts, sum as f64 / 1e9)
    }
}

struct Series {
    status: StatusCounts,
    latency: Option<Latency>,
}

// ---------------------------------------------------------------------------
// Methods

/// GET POST PUT PATCH DELETE HEAD OPTIONS CONNECT TRACE, the route's own
/// non-standard method, and everything else (`_OTHER`).
const METHOD_SLOTS: usize = 11;
const CUSTOM: usize = 9;
const OTHER: usize = 10;
const STANDARD: [&str; 9] = [
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "CONNECT", "TRACE",
];

/// Request methods are bounded to the standard ones plus the route's own,
/// so arbitrary methods sent to `any` routes cannot explode cardinality.
#[inline]
fn method_slot(m: &Method, route_method: Option<&Method>) -> usize {
    match *m {
        Method::GET => 0,
        Method::POST => 1,
        Method::PUT => 2,
        Method::PATCH => 3,
        Method::DELETE => 4,
        Method::HEAD => 5,
        Method::OPTIONS => 6,
        Method::CONNECT => 7,
        Method::TRACE => 8,
        _ if route_method == Some(m) => CUSTOM,
        _ => OTHER,
    }
}

fn method_label(slot: usize, route_method: Option<&Method>) -> &str {
    match slot {
        CUSTOM => route_method.map_or("_OTHER", Method::as_str),
        OTHER => "_OTHER",
        i => STANDARD[i],
    }
}

/// One route's series, by request method (created on first use, or at build
/// time for the route's own method).
struct RouteEntry {
    route: &'static str,
    method: Option<Method>,
    info: Option<&'static RouteInfo>,
    excluded: bool,
    slots: [OnceLock<Box<Series>>; METHOD_SLOTS],
}

impl RouteEntry {
    fn new(route: &'static str, info: Option<&'static RouteInfo>, excluded: bool) -> Self {
        RouteEntry {
            route,
            method: info.and_then(|i| i.method().cloned()),
            info,
            excluded,
            slots: std::array::from_fn(|_| OnceLock::new()),
        }
    }

    #[inline]
    fn series(&self, slot: usize, latency_buckets: Option<usize>) -> &Series {
        self.slots[slot].get_or_init(|| {
            Box::new(Series {
                status: StatusCounts::new(),
                latency: latency_buckets.map(Latency::new),
            })
        })
    }
}

/// Route address -> entry, by open addressing. Built once in `on_build`.
struct RouteIndex {
    entries: Box<[RouteEntry]>,
    table: Box<[u32]>,
    mask: usize,
}

#[inline]
fn addr_hash(info: &'static RouteInfo) -> usize {
    let a = info as *const RouteInfo as usize;
    (a >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15_u64 as usize) >> 7
}

impl RouteIndex {
    fn new(entries: Vec<RouteEntry>) -> Self {
        let cap = (entries.len() * 2).next_power_of_two().max(8);
        let mask = cap - 1;
        let mut table = vec![0u32; cap].into_boxed_slice();
        for (i, e) in entries.iter().enumerate() {
            let Some(info) = e.info else { continue };
            let mut h = addr_hash(info) & mask;
            while table[h] != 0 {
                h = (h + 1) & mask;
            }
            table[h] = i as u32 + 1;
        }
        RouteIndex {
            entries: entries.into_boxed_slice(),
            table,
            mask,
        }
    }

    #[inline]
    fn get(&self, info: &'static RouteInfo) -> Option<&RouteEntry> {
        let mut h = addr_hash(info) & self.mask;
        loop {
            let i = self.table[h];
            if i == 0 {
                return None;
            }
            let e = &self.entries[i as usize - 1];
            if e.info.is_some_and(|x| std::ptr::eq(x, info)) {
                return Some(e);
            }
            h = (h + 1) & self.mask;
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration & state

/// Exact path, or a prefix when the pattern ends with `*`.
#[derive(Clone, Debug)]
pub(crate) enum PathPattern {
    Exact(String),
    Prefix(String),
}

impl PathPattern {
    pub(crate) fn parse(s: &str) -> Self {
        match s.strip_suffix('*') {
            Some(p) => PathPattern::Prefix(p.to_owned()),
            None => PathPattern::Exact(s.to_owned()),
        }
    }

    pub(crate) fn matches(&self, path: &str) -> bool {
        match self {
            PathPattern::Exact(p) => p == path,
            PathPattern::Prefix(p) => path.starts_with(p.as_str()),
        }
    }
}

pub(crate) struct HttpConfig {
    pub(crate) prefix: String,
    pub(crate) buckets: Vec<f64>,
    pub(crate) exclude: Vec<PathPattern>,
    pub(crate) track_unmatched: bool,
}

/// Per-app HTTP metrics. Leaked: lives as long as the route table.
pub(crate) struct HttpMetrics {
    total: String,
    duration: String,
    in_flight_name: String,
    bounds: Box<[u64]>,
    le: Box<[String]>,
    exclude: Vec<PathPattern>,
    track_unmatched: bool,
    routes: OnceLock<RouteIndex>,
    /// Routes not seen at build time (should not happen; kept for safety).
    late: Mutex<Vec<&'static RouteEntry>>,
    unmatched: RouteEntry,
    in_flight: Box<[PaddedI64]>,
}

/// Marks responses the route middleware has already counted, so the global
/// middleware does not count them again as `unmatched`. Only inserted on
/// 404/405 responses, so the hot path never allocates for it.
#[derive(Clone, Copy)]
struct Counted;

impl HttpMetrics {
    pub(crate) fn new(cfg: HttpConfig) -> &'static HttpMetrics {
        let name = |s: &str| {
            if cfg.prefix.is_empty() {
                s.to_owned()
            } else {
                format!("{}_{s}", cfg.prefix)
            }
        };
        let m = HttpMetrics {
            total: name("requests_total"),
            duration: name("request_duration_seconds"),
            in_flight_name: name("requests_in_flight"),
            bounds: cfg
                .buckets
                .iter()
                .map(|s| (s * 1e9).round().max(0.0) as u64)
                .collect(),
            le: le_labels(&cfg.buckets),
            exclude: cfg.exclude,
            track_unmatched: cfg.track_unmatched,
            routes: OnceLock::new(),
            late: Mutex::new(Vec::new()),
            unmatched: RouteEntry::new("unmatched", None, false),
            in_flight: (0..shard_count())
                .map(|_| PaddedI64(AtomicI64::new(0)))
                .collect(),
        };
        Box::leak(Box::new(m))
    }

    fn excluded(&self, path: &str) -> bool {
        self.exclude.iter().any(|p| p.matches(path))
    }

    /// Pre-register a series for every route (called from `on_build`).
    pub(crate) fn build(&self, table: &[&'static RouteInfo]) {
        let entries: Vec<RouteEntry> = table
            .iter()
            .map(|info| {
                let e = RouteEntry::new(info.path(), Some(info), self.excluded(info.path()));
                if let (false, Some(m)) = (e.excluded, info.method()) {
                    e.series(method_slot(m, Some(m)), Some(self.bounds.len()));
                }
                e
            })
            .collect();
        let _ = self.routes.set(RouteIndex::new(entries));
    }

    #[inline]
    fn entry(&self, info: &'static RouteInfo) -> &RouteEntry {
        if let Some(e) = self.routes.get().and_then(|r| r.get(info)) {
            return e;
        }
        self.late_entry(info)
    }

    #[cold]
    fn late_entry(&self, info: &'static RouteInfo) -> &'static RouteEntry {
        let mut late = self.late.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = late
            .iter()
            .find(|e| e.info.is_some_and(|x| std::ptr::eq(x, info)))
        {
            return e;
        }
        let e: &'static RouteEntry = Box::leak(Box::new(RouteEntry::new(
            info.path(),
            Some(info),
            self.excluded(info.path()),
        )));
        late.push(e);
        e
    }

    #[inline]
    fn in_flight_add(&self, d: i64) {
        self.in_flight[shard()].0.fetch_add(d, Relaxed);
    }

    // ---- rendering ------------------------------------------------------

    fn all_entries(&self) -> Vec<&RouteEntry> {
        let mut v: Vec<&RouteEntry> = self
            .routes
            .get()
            .map(|r| r.entries.iter().collect())
            .unwrap_or_default();
        v.extend(
            self.late
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .copied(),
        );
        v.push(&self.unmatched);
        v
    }

    pub(crate) fn encode(&self, out: &mut Encoder<'_>) {
        let entries = self.all_entries();

        out.header(
            &self.total,
            "Total number of HTTP requests.",
            MetricKind::Counter,
        );
        for e in &entries {
            for (slot, s) in e.slots.iter().enumerate() {
                let Some(s) = s.get() else { continue };
                let method = method_label(slot, e.method.as_ref());
                for (code, n) in s.status.snapshot() {
                    let code = code.to_string();
                    let labels = [("method", method), ("route", e.route), ("status", &code)];
                    out.write(&self.total, "", &labels, Num::U(n));
                }
            }
        }

        out.header(
            &self.duration,
            "HTTP request latency in seconds.",
            MetricKind::Histogram,
        );
        for e in &entries {
            for (slot, s) in e.slots.iter().enumerate() {
                let Some(lat) = s.get().and_then(|s| s.latency.as_ref()) else {
                    continue;
                };
                let (counts, sum) = lat.snapshot(self.bounds.len());
                if counts.iter().all(|&c| c == 0) {
                    continue;
                }
                let labels = [
                    ("method", method_label(slot, e.method.as_ref())),
                    ("route", e.route),
                ];
                encode_histogram(out, &self.duration, &labels, &self.le, &counts, sum);
            }
        }

        out.header(
            &self.in_flight_name,
            "HTTP requests currently being served.",
            MetricKind::Gauge,
        );
        let n: i64 = self.in_flight.iter().map(|c| c.0.load(Relaxed)).sum();
        out.write(&self.in_flight_name, "", &[], Num::I(n.max(0)));
    }
}

// ---------------------------------------------------------------------------
// Middleware

/// Records one request into a series; on drop (cancellation or panic) it
/// still releases the in-flight slot, and counts a panic as a 500.
struct Track {
    m: &'static HttpMetrics,
    series: &'static Series,
    start: Instant,
    in_flight: bool,
    done: bool,
}

impl Track {
    fn record(&mut self, status: u16) {
        self.done = true;
        let shard = shard();
        self.series.status.record(shard, status);
        if let Some(lat) = &self.series.latency {
            let nanos = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
            lat.observe(&self.m.bounds, shard, nanos);
        }
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        if !self.done && std::thread::panicking() {
            self.record(500);
        }
        if self.in_flight {
            self.m.in_flight_add(-1);
        }
    }
}

/// After routing: counts matched requests by their route template.
pub(crate) struct RouteLayer(pub(crate) &'static HttpMetrics);

impl Middleware for RouteLayer {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let m = self.0;
        let Some(info) = req.route() else {
            return next.run(req);
        };
        let entry: &'static RouteEntry = m.entry(info);
        if entry.excluded {
            return next.run(req);
        }
        let slot = method_slot(req.method(), entry.method.as_ref());
        let series = entry.series(slot, Some(m.bounds.len()));
        let in_flight = !m.track_unmatched;
        if in_flight {
            m.in_flight_add(1);
        }
        let mut track = Track {
            m,
            series,
            start: Instant::now(),
            in_flight,
            done: false,
        };
        let fut = next.run(req);
        Box::pin(async move {
            let mut res = fut.await;
            let status = res.status().as_u16();
            track.record(status);
            if status == 404 || status == 405 {
                res.extensions_mut().insert(Counted);
            }
            res
        })
    }
}

/// Before routing: tracks in-flight requests and counts 404/405 responses
/// the route middleware never saw under `route="unmatched"`.
pub(crate) struct GlobalLayer(pub(crate) &'static HttpMetrics);

impl Middleware for GlobalLayer {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let m = self.0;
        if !m.exclude.is_empty() && m.excluded(req.path()) {
            return next.run(req);
        }
        let slot = method_slot(req.method(), None);
        m.in_flight_add(1);
        let guard = InFlight(m);
        let fut = next.run(req);
        Box::pin(async move {
            let res = fut.await;
            drop(guard);
            let status = res.status().as_u16();
            if (status == 404 || status == 405) && res.extensions().get::<Counted>().is_none() {
                m.unmatched
                    .series(slot, None)
                    .status
                    .record(shard(), status);
            }
            res
        })
    }
}

struct InFlight(&'static HttpMetrics);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight_add(-1);
    }
}
