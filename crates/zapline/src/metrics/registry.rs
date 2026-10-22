//! Custom metrics: [`Registry`], [`Counter`], [`Gauge`], [`Histogram`],
//! scrape-time [`Collector`]s and the Prometheus text [`Encoder`].

use std::any::Any;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

/// Default histogram buckets in seconds (5 ms .. 10 s), the same as the
/// official Prometheus clients.
pub const DEFAULT_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Maximum number of label combinations per metric family. Further
/// combinations are folded into one series whose label values are all
/// `"_overflow"`, so unbounded label values cannot exhaust memory.
pub const MAX_SERIES_PER_FAMILY: usize = 10_000;

const OVERFLOW: &str = "_overflow";

/// The `# TYPE` of a metric family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
    Untyped,
}

impl MetricKind {
    /// The name used on the `# TYPE` line.
    pub fn as_str(self) -> &'static str {
        match self {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Histogram => "histogram",
            MetricKind::Untyped => "untyped",
        }
    }
}

// ---------------------------------------------------------------------------
// Text exposition format 0.0.4

/// Writes the Prometheus text exposition format (version 0.0.4).
///
/// Handed to [`Collector::collect`]; label values and help texts are escaped
/// for you.
pub struct Encoder<'a> {
    buf: &'a mut String,
}

/// A sample value: integers are written exactly, floats in Prometheus syntax.
#[derive(Clone, Copy)]
pub(crate) enum Num {
    U(u64),
    I(i64),
    F(f64),
}

impl<'a> Encoder<'a> {
    /// An encoder appending to `buf`.
    pub fn new(buf: &'a mut String) -> Self {
        Encoder { buf }
    }

    /// Write the `# HELP` and `# TYPE` lines that start a metric family.
    pub fn header(&mut self, name: &str, help: &str, kind: MetricKind) {
        let b = &mut *self.buf;
        b.push_str("# HELP ");
        b.push_str(name);
        b.push(' ');
        escape_help(b, help);
        b.push_str("\n# TYPE ");
        b.push_str(name);
        b.push(' ');
        b.push_str(kind.as_str());
        b.push('\n');
    }

    /// Write one sample line: `name{label="value",...} value`.
    pub fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.write(name, "", labels, Num::F(value));
    }

    /// Write one sample line with an exact integer value.
    pub fn sample_u64(&mut self, name: &str, labels: &[(&str, &str)], value: u64) {
        self.write(name, "", labels, Num::U(value));
    }

    pub(crate) fn write(&mut self, name: &str, suffix: &str, labels: &[(&str, &str)], v: Num) {
        let b = &mut *self.buf;
        b.push_str(name);
        b.push_str(suffix);
        if !labels.is_empty() {
            b.push('{');
            for (i, (k, val)) in labels.iter().enumerate() {
                if i > 0 {
                    b.push(',');
                }
                b.push_str(k);
                b.push_str("=\"");
                escape_label_value(b, val);
                b.push('"');
            }
            b.push('}');
        }
        b.push(' ');
        write_num(b, v);
        b.push('\n');
    }
}

fn write_num(b: &mut String, v: Num) {
    match v {
        Num::U(n) => {
            let _ = write!(b, "{n}");
        }
        Num::I(n) => {
            let _ = write!(b, "{n}");
        }
        Num::F(f) => write_float(b, f),
    }
}

/// Floats in Prometheus syntax (`+Inf`, `-Inf`, `NaN`).
pub(crate) fn write_float(b: &mut String, f: f64) {
    if f.is_nan() {
        b.push_str("NaN");
    } else if f.is_infinite() {
        b.push_str(if f > 0.0 { "+Inf" } else { "-Inf" });
    } else {
        let _ = write!(b, "{f}");
    }
}

pub(crate) fn format_float(f: f64) -> String {
    let mut s = String::new();
    write_float(&mut s, f);
    s
}

/// Label values escape `\`, `"` and newlines.
pub(crate) fn escape_label_value(b: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\\' => b.push_str("\\\\"),
            '"' => b.push_str("\\\""),
            '\n' => b.push_str("\\n"),
            c => b.push(c),
        }
    }
}

/// Help texts escape `\` and newlines.
fn escape_help(b: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\\' => b.push_str("\\\\"),
            '\n' => b.push_str("\\n"),
            c => b.push(c),
        }
    }
}

/// `[a-zA-Z_:][a-zA-Z0-9_:]*`
pub fn is_valid_metric_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == ':')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

/// `[a-zA-Z_][a-zA-Z0-9_]*`, not starting with the reserved `__`.
pub fn is_valid_label_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with("__")
}

/// Validate and normalize histogram bucket bounds: finite, strictly
/// increasing; a trailing `+Inf` is implied and removed.
///
/// # Panics
/// On NaN, `-Inf`, or unsorted / duplicate bounds (a configuration error).
pub(crate) fn check_buckets(buckets: &[f64]) -> Vec<f64> {
    let mut out: Vec<f64> = buckets.to_vec();
    if out.last() == Some(&f64::INFINITY) {
        out.pop();
    }
    for (i, b) in out.iter().enumerate() {
        assert!(
            b.is_finite(),
            "histogram bucket bounds must be finite, got {b}"
        );
        if i > 0 {
            assert!(
                out[i - 1] < *b,
                "histogram buckets must be strictly increasing: {:?}",
                buckets
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Collectors

/// Metrics computed at scrape time (process stats, queue depths, pool sizes).
///
/// ```
/// use zapline::metrics::{Collector, Encoder, MetricKind, Registry};
///
/// struct QueueDepth;
/// impl Collector for QueueDepth {
///     fn collect(&self, out: &mut Encoder<'_>) {
///         out.header("jobs_queued", "Jobs waiting to run.", MetricKind::Gauge);
///         out.sample("jobs_queued", &[("queue", "email")], 3.0);
///     }
/// }
///
/// let registry = Registry::new();
/// registry.register(QueueDepth);
/// assert!(registry.render().contains("jobs_queued{queue=\"email\"} 3"));
/// ```
pub trait Collector: Send + Sync + 'static {
    /// Write one or more complete metric families.
    fn collect(&self, out: &mut Encoder<'_>);
}

impl<F: Fn(&mut Encoder<'_>) + Send + Sync + 'static> Collector for F {
    fn collect(&self, out: &mut Encoder<'_>) {
        self(out)
    }
}

// ---------------------------------------------------------------------------
// Series storage

trait Core: Sized + Send + Sync + 'static {
    type Config: Send + Sync + 'static;
    const KIND: MetricKind;
    fn new(cfg: &Self::Config) -> Self;
    fn encode(
        &self,
        cfg: &Self::Config,
        out: &mut Encoder<'_>,
        name: &str,
        labels: &[(&str, &str)],
    );
}

#[doc(hidden)]
pub struct CounterCore(AtomicU64);

impl Core for CounterCore {
    type Config = ();
    const KIND: MetricKind = MetricKind::Counter;
    fn new(_: &()) -> Self {
        CounterCore(AtomicU64::new(0))
    }
    fn encode(&self, _: &(), out: &mut Encoder<'_>, name: &str, labels: &[(&str, &str)]) {
        out.write(name, "", labels, Num::U(self.0.load(Relaxed)));
    }
}

/// Adds `v` to an `f64` stored as bits in an `AtomicU64` (a CAS loop, so it
/// works on every supported toolchain without `fetch_update`/`try_update`).
fn atomic_add_f64(cell: &AtomicU64, v: f64) {
    let mut cur = cell.load(Relaxed);
    loop {
        let new = (f64::from_bits(cur) + v).to_bits();
        match cell.compare_exchange_weak(cur, new, Relaxed, Relaxed) {
            Ok(_) => return,
            Err(actual) => cur = actual,
        }
    }
}

#[doc(hidden)]
pub struct GaugeCore(AtomicU64);

impl GaugeCore {
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Relaxed))
    }
    fn add(&self, v: f64) {
        atomic_add_f64(&self.0, v);
    }
}

impl Core for GaugeCore {
    type Config = ();
    const KIND: MetricKind = MetricKind::Gauge;
    fn new(_: &()) -> Self {
        GaugeCore(AtomicU64::new(0f64.to_bits()))
    }
    fn encode(&self, _: &(), out: &mut Encoder<'_>, name: &str, labels: &[(&str, &str)]) {
        out.write(name, "", labels, Num::F(self.get()));
    }
}

#[doc(hidden)]
pub struct HistogramCore {
    /// Per-bucket (non-cumulative) counts; the last one is `+Inf`.
    counts: Box<[AtomicU64]>,
    sum: AtomicU64,
}

#[doc(hidden)]
pub struct HistogramConfig {
    bounds: Box<[f64]>,
    le: Box<[String]>,
}

impl HistogramCore {
    fn observe(&self, bounds: &[f64], v: f64) {
        if v.is_nan() {
            return;
        }
        let i = bounds.partition_point(|&b| b < v);
        self.counts[i].fetch_add(1, Relaxed);
        atomic_add_f64(&self.sum, v);
    }
    fn count(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Relaxed)).sum()
    }
    fn sum(&self) -> f64 {
        f64::from_bits(self.sum.load(Relaxed))
    }
}

impl Core for HistogramCore {
    type Config = HistogramConfig;
    const KIND: MetricKind = MetricKind::Histogram;
    fn new(cfg: &HistogramConfig) -> Self {
        HistogramCore {
            counts: (0..=cfg.bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum: AtomicU64::new(0f64.to_bits()),
        }
    }
    fn encode(
        &self,
        cfg: &HistogramConfig,
        out: &mut Encoder<'_>,
        name: &str,
        labels: &[(&str, &str)],
    ) {
        let counts: Vec<u64> = self.counts.iter().map(|c| c.load(Relaxed)).collect();
        encode_histogram(out, name, labels, &cfg.le, &counts, self.sum());
    }
}

/// Write `_bucket` (cumulative, ending in `+Inf`), `_sum` and `_count`.
/// `counts` are per-bucket counts with `+Inf` last; `le` matches them.
pub(crate) fn encode_histogram(
    out: &mut Encoder<'_>,
    name: &str,
    labels: &[(&str, &str)],
    le: &[String],
    counts: &[u64],
    sum: f64,
) {
    let mut with_le: Vec<(&str, &str)> = Vec::with_capacity(labels.len() + 1);
    with_le.extend_from_slice(labels);
    with_le.push(("le", ""));
    let mut cumulative = 0u64;
    for (bound, n) in le.iter().zip(counts) {
        cumulative += n;
        *with_le.last_mut().unwrap() = ("le", bound);
        out.write(name, "_bucket", &with_le, Num::U(cumulative));
    }
    out.write(name, "_sum", labels, Num::F(sum));
    out.write(name, "_count", labels, Num::U(cumulative));
}

/// `le` label values for `bounds`, plus `+Inf`.
pub(crate) fn le_labels(bounds: &[f64]) -> Box<[String]> {
    bounds
        .iter()
        .map(|b| format_float(*b))
        .chain(std::iter::once("+Inf".to_owned()))
        .collect()
}

struct Children<C> {
    index: HashMap<String, usize>,
    list: Vec<(Box<[String]>, Arc<C>)>,
}

struct Family<C: Core> {
    name: String,
    help: String,
    labels: Box<[String]>,
    cfg: C::Config,
    children: RwLock<Children<C>>,
}

/// Length-prefixed so `["a,b"]` and `["a", "b"]` never collide.
fn child_key<S: AsRef<str>>(values: &[S]) -> String {
    let mut key = String::new();
    for v in values {
        let v = v.as_ref();
        let _ = write!(key, "{}:{v}", v.len());
    }
    key
}

impl<C: Core> Family<C> {
    fn new(name: &str, help: &str, labels: &[&str], cfg: C::Config) -> Self {
        Family {
            name: name.to_owned(),
            help: help.to_owned(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            cfg,
            children: RwLock::new(Children {
                index: HashMap::new(),
                list: Vec::new(),
            }),
        }
    }

    /// The series for `values` (padded with `""` / truncated to the number of
    /// label names), created on first use.
    fn child(&self, values: &[&str]) -> Arc<C> {
        let n = self.labels.len();
        let mut vals: Vec<&str> = values.iter().copied().take(n).collect();
        vals.resize(n, "");
        let key = child_key(&vals);
        {
            let r = read(&self.children);
            if let Some(&i) = r.index.get(&key) {
                return r.list[i].1.clone();
            }
        }
        let mut w = write(&self.children);
        if let Some(&i) = w.index.get(&key) {
            return w.list[i].1.clone();
        }
        let (vals, key) = if w.list.len() >= MAX_SERIES_PER_FAMILY {
            let overflow = vec![OVERFLOW; n];
            let key = child_key(&overflow);
            if let Some(&i) = w.index.get(&key) {
                return w.list[i].1.clone();
            }
            (overflow, key)
        } else {
            (vals, key)
        };
        let core = Arc::new(C::new(&self.cfg));
        w.list
            .push((vals.iter().map(|s| s.to_string()).collect(), core.clone()));
        let i = w.list.len() - 1;
        w.index.insert(key, i);
        core
    }
}

fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

/// Type-erased family stored in a registry.
trait AnyFamily: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn kind(&self) -> MetricKind;
    fn label_names(&self) -> &[String];
    fn encode(&self, out: &mut Encoder<'_>);
    fn as_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;
}

impl<C: Core> AnyFamily for Family<C> {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> MetricKind {
        C::KIND
    }
    fn label_names(&self) -> &[String] {
        &self.labels
    }
    fn encode(&self, out: &mut Encoder<'_>) {
        out.header(&self.name, &self.help, C::KIND);
        let children = read(&self.children);
        for (values, core) in &children.list {
            let labels: Vec<(&str, &str)> = self
                .labels
                .iter()
                .map(String::as_str)
                .zip(values.iter().map(String::as_str))
                .collect();
            core.encode(&self.cfg, out, &self.name, &labels);
        }
    }
    fn as_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

// ---------------------------------------------------------------------------
// Handles

macro_rules! handle_common {
    ($ty:ident, $core:ident) => {
        impl $ty {
            /// The series for these label values (in label-name order), created
            /// on first use. Missing values are treated as `""` and extra ones
            /// are ignored. Keep the returned handle for hot paths: the lookup
            /// hashes the values.
            pub fn with_labels(&self, values: &[&str]) -> $ty {
                $ty {
                    family: self.family.clone(),
                    core: Some(self.family.child(values)),
                }
            }

            fn core(&self) -> std::borrow::Cow<'_, Arc<$core>> {
                match &self.core {
                    Some(c) => std::borrow::Cow::Borrowed(c),
                    None => std::borrow::Cow::Owned(self.family.child(&[])),
                }
            }
        }

        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($ty))
                    .field("name", &self.family.name)
                    .finish_non_exhaustive()
            }
        }
    };
}

/// A monotonically increasing integer counter.
///
/// ```
/// let jobs = zapline::metrics::Registry::new()
///     .counter("jobs_total", "Jobs processed.", &["kind"]);
/// jobs.with_labels(&["email"]).inc();
/// jobs.with_labels(&["sms"]).add(2);
/// assert_eq!(jobs.with_labels(&["sms"]).get(), 2);
/// ```
#[derive(Clone)]
pub struct Counter {
    family: Arc<Family<CounterCore>>,
    core: Option<Arc<CounterCore>>,
}
handle_common!(Counter, CounterCore);

impl Counter {
    #[inline]
    pub fn inc(&self) {
        self.add(1);
    }
    #[inline]
    pub fn add(&self, n: u64) {
        self.core().0.fetch_add(n, Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.core().0.load(Relaxed)
    }
}

/// A value that can go up and down (stored as `f64`).
///
/// ```
/// let conns = zapline::metrics::Registry::new()
///     .gauge("ws_connections", "Open WebSocket connections.", &[]);
/// conns.inc();
/// conns.add(2.0);
/// conns.dec();
/// assert_eq!(conns.get(), 2.0);
/// ```
#[derive(Clone)]
pub struct Gauge {
    family: Arc<Family<GaugeCore>>,
    core: Option<Arc<GaugeCore>>,
}
handle_common!(Gauge, GaugeCore);

impl Gauge {
    pub fn set(&self, v: f64) {
        self.core().0.store(v.to_bits(), Relaxed);
    }
    pub fn add(&self, v: f64) {
        self.core().add(v);
    }
    pub fn sub(&self, v: f64) {
        self.core().add(-v);
    }
    pub fn inc(&self) {
        self.add(1.0);
    }
    pub fn dec(&self) {
        self.add(-1.0);
    }
    pub fn get(&self) -> f64 {
        self.core().get()
    }
}

/// A histogram of observed values (latencies, sizes).
///
/// ```
/// let h = zapline::metrics::Registry::new()
///     .histogram("db_query_seconds", "Query latency.", &["table"]);
/// let users = h.with_labels(&["users"]);
/// users.observe(0.003);
/// {
///     let _t = users.start_timer(); // observes the elapsed time on drop
/// }
/// assert_eq!(users.count(), 2);
/// ```
#[derive(Clone)]
pub struct Histogram {
    family: Arc<Family<HistogramCore>>,
    core: Option<Arc<HistogramCore>>,
}
handle_common!(Histogram, HistogramCore);

impl Histogram {
    pub fn observe(&self, v: f64) {
        self.core().observe(&self.family.cfg.bounds, v);
    }
    /// Observe the time until the returned guard is dropped or stopped.
    pub fn start_timer(&self) -> HistogramTimer {
        HistogramTimer {
            histogram: self.clone(),
            start: Instant::now(),
            done: false,
        }
    }
    /// Number of observations.
    pub fn count(&self) -> u64 {
        self.core().count()
    }
    /// Sum of observed values.
    pub fn sum(&self) -> f64 {
        self.core().sum()
    }
}

/// Observes elapsed seconds into a [`Histogram`] when dropped or stopped.
#[must_use = "the timer observes when dropped; bind it to a variable"]
pub struct HistogramTimer {
    histogram: Histogram,
    start: Instant,
    done: bool,
}

impl HistogramTimer {
    /// Observe now and return the elapsed seconds.
    pub fn stop(mut self) -> f64 {
        self.finish()
    }
    /// Drop without observing.
    pub fn discard(mut self) {
        self.done = true;
    }
    fn finish(&mut self) -> f64 {
        let secs = self.start.elapsed().as_secs_f64();
        if !self.done {
            self.done = true;
            self.histogram.observe(secs);
        }
        secs
    }
}

impl Drop for HistogramTimer {
    fn drop(&mut self) {
        self.finish();
    }
}

// ---------------------------------------------------------------------------
// Registry

#[derive(Default)]
struct RegistryInner {
    families: RwLock<Vec<Arc<dyn AnyFamily>>>,
    collectors: RwLock<Vec<Arc<dyn Collector>>>,
}

/// A set of metrics rendered together at scrape time.
///
/// Use the process-wide [`registry()`] (what [`Metrics`](super::Metrics)
/// exposes by default) or create your own. Cloning is cheap; clones share
/// the same metrics.
///
/// Registering a name again with the same type and label names returns the
/// existing metric, so it is safe to "register" lazily.
///
/// ```
/// use zapline::metrics;
///
/// let signups = metrics::registry().counter("signups_total", "New accounts.", &["plan"]);
/// signups.with_labels(&["pro"]).inc();
/// assert!(metrics::registry().render().contains("signups_total{plan=\"pro\"} 1"));
/// ```
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<RegistryInner>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = read(&self.inner.families)
            .iter()
            .map(|f| f.name().to_owned())
            .collect();
        f.debug_struct("Registry")
            .field("families", &names)
            .finish()
    }
}

/// The process-wide default registry.
pub fn registry() -> &'static Registry {
    static GLOBAL: OnceLock<Registry> = OnceLock::new();
    GLOBAL.get_or_init(Registry::new)
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or look up) a counter.
    ///
    /// # Panics
    /// On an invalid metric or label name, or if `name` is already registered
    /// with a different type or label names.
    pub fn counter(&self, name: &str, help: &str, labels: &[&str]) -> Counter {
        let family = self.family::<CounterCore>(name, help, labels, ());
        let core = labels.is_empty().then(|| family.child(&[]));
        Counter { family, core }
    }

    /// Register (or look up) a gauge. Panics like [`counter`](Self::counter).
    pub fn gauge(&self, name: &str, help: &str, labels: &[&str]) -> Gauge {
        let family = self.family::<GaugeCore>(name, help, labels, ());
        let core = labels.is_empty().then(|| family.child(&[]));
        Gauge { family, core }
    }

    /// Register (or look up) a histogram with [`DEFAULT_BUCKETS`].
    /// Panics like [`counter`](Self::counter).
    pub fn histogram(&self, name: &str, help: &str, labels: &[&str]) -> Histogram {
        self.histogram_with_buckets(name, help, labels, DEFAULT_BUCKETS)
    }

    /// Register (or look up) a histogram with custom bucket upper bounds.
    ///
    /// # Panics
    /// Like [`counter`](Self::counter), on a `le` label, or on invalid buckets
    /// (non-finite or not strictly increasing).
    pub fn histogram_with_buckets(
        &self,
        name: &str,
        help: &str,
        labels: &[&str],
        buckets: &[f64],
    ) -> Histogram {
        assert!(
            !labels.contains(&"le"),
            "metric `{name}`: `le` is reserved for histogram buckets"
        );
        let bounds = check_buckets(buckets);
        let cfg = HistogramConfig {
            le: le_labels(&bounds),
            bounds: bounds.into(),
        };
        let family = self.family::<HistogramCore>(name, help, labels, cfg);
        let core = labels.is_empty().then(|| family.child(&[]));
        Histogram { family, core }
    }

    /// Add a scrape-time [`Collector`].
    pub fn register(&self, collector: impl Collector) {
        write(&self.inner.collectors).push(Arc::new(collector));
    }

    /// Render every metric in the Prometheus text format.
    pub fn render(&self) -> String {
        let mut buf = String::with_capacity(1024);
        self.encode(&mut Encoder::new(&mut buf));
        buf
    }

    /// Append every metric to `out`.
    pub fn encode(&self, out: &mut Encoder<'_>) {
        let families: Vec<Arc<dyn AnyFamily>> = read(&self.inner.families).clone();
        for f in families {
            f.encode(out);
        }
        let collectors: Vec<Arc<dyn Collector>> = read(&self.inner.collectors).clone();
        for c in collectors {
            c.collect(out);
        }
    }

    fn family<C: Core>(
        &self,
        name: &str,
        help: &str,
        labels: &[&str],
        cfg: C::Config,
    ) -> Arc<Family<C>> {
        assert!(is_valid_metric_name(name), "invalid metric name `{name}`");
        for (i, l) in labels.iter().enumerate() {
            assert!(
                is_valid_label_name(l),
                "metric `{name}`: invalid label name `{l}`"
            );
            assert!(
                !labels[..i].contains(l),
                "metric `{name}`: duplicate label name `{l}`"
            );
        }
        let mut families = write(&self.inner.families);
        if let Some(existing) = families.iter().find(|f| f.name() == name) {
            assert!(
                existing.kind() == C::KIND
                    && existing
                        .label_names()
                        .iter()
                        .map(String::as_str)
                        .eq(labels.iter().copied()),
                "metric `{name}` is already registered as a {} with labels {:?}",
                existing.kind().as_str(),
                existing.label_names()
            );
            return existing
                .clone()
                .as_any()
                .downcast::<Family<C>>()
                .unwrap_or_else(|_| unreachable!("metric kinds map to one core type"));
        }
        let family = Arc::new(Family::<C>::new(name, help, labels, cfg));
        families.push(family.clone());
        family
    }
}
