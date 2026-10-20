//! Rate limiting with GCRA (a smooth token bucket) over a sharded,
//! bounded in-memory store.
//!
//! # Global vs. per-route limits
//!
//! - [`App::wrap`](crate::App::wrap) runs before routing: one limit for
//!   every request, 404s included. Route policies are not visible there.
//! - [`App::wrap_route`](crate::App::wrap_route) / [`Routes::wrap`] run
//!   after routing: the middleware's limit is the default, and routes
//!   annotated with [`Routes::rate_limit`] / [`App::rate_limit`] (or a
//!   [`RateLimitPolicy`] in their metadata) get their own limit and their
//!   own buckets.
//!
//! ```
//! use std::time::Duration;
//! use zapline::prelude::*;
//! use zapline::middleware::RateLimit;
//!
//! let min = Duration::from_secs(60);
//! let app = App::new()
//!     .post("/login", || async { "welcome" })
//!     .rate_limit(5, min) // 5 login attempts per minute per client
//!     .get("/items", || async { "items" })
//!     .wrap_route(RateLimit::per_ip(300, min)); // everything else
//! ```
//!
//! Limited requests get `429 Too Many Requests` with code `"rate_limited"`
//! and `Retry-After`. Every response from a limited route carries
//! `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset`.

use std::collections::HashMap;
use std::fmt;
use std::hash::{BuildHasher, BuildHasherDefault, Hash, Hasher, RandomState};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use http::header::{HeaderName, HeaderValue, RETRY_AFTER};

use super::{Middleware, Next};
use crate::app::App;
use crate::handler::BoxFuture;
use crate::request::Request;
use crate::response::{IntoResponse, Response};
use crate::router::{RouteInfo, Routes};
use crate::Error;

/// `RateLimit-Limit`
pub const RATELIMIT_LIMIT: HeaderName = HeaderName::from_static("ratelimit-limit");
/// `RateLimit-Remaining`
pub const RATELIMIT_REMAINING: HeaderName = HeaderName::from_static("ratelimit-remaining");
/// `RateLimit-Reset`
pub const RATELIMIT_RESET: HeaderName = HeaderName::from_static("ratelimit-reset");

const SHARDS: usize = 64;
const DEFAULT_MAX_KEYS: usize = 100_000;
const SWEEP_EVERY_NS: u64 = 10_000_000_000;

// ---------------------------------------------------------------------------
// Quota (GCRA parameters)

/// GCRA parameters: one request is "worth" `interval`; up to `tolerance`
/// worth of requests may be outstanding at once.
#[derive(Clone, Copy, Debug)]
struct Quota {
    limit: u32,
    interval: u64,
    tolerance: u64,
}

struct Outcome {
    allowed: bool,
    remaining: u64,
    reset_ns: u64,
    retry_ns: u64,
}

impl Quota {
    fn new(limit: u32, period: Duration) -> Self {
        assert!(limit > 0, "rate limit must be at least 1 request");
        assert!(!period.is_zero(), "rate limit period must be non-zero");
        let interval = (period.as_nanos() / u128::from(limit)).clamp(1, u128::from(u64::MAX));
        let interval = interval as u64;
        Quota {
            limit,
            interval,
            tolerance: interval.saturating_mul(u64::from(limit)),
        }
    }

    fn burst(mut self, burst: u32) -> Self {
        assert!(burst > 0, "rate limit burst must be at least 1");
        self.tolerance = self.interval.saturating_mul(u64::from(burst));
        self
    }

    /// Decide for a key whose theoretical arrival time is `stored` (0 =
    /// unknown). Returns the new TAT to store when the request is allowed.
    #[inline]
    fn decide(&self, stored: u64, now: u64) -> (Outcome, Option<u64>) {
        let tat = stored.max(now);
        let new_tat = tat.saturating_add(self.interval);
        let allow_at = new_tat.saturating_sub(self.tolerance);
        if now < allow_at {
            let out = Outcome {
                allowed: false,
                remaining: 0,
                reset_ns: tat - now,
                retry_ns: allow_at - now,
            };
            return (out, None);
        }
        let used = new_tat - now;
        let out = Outcome {
            allowed: true,
            remaining: self.tolerance.saturating_sub(used) / self.interval,
            reset_ns: used,
            retry_ns: 0,
        };
        (out, Some(new_tat))
    }
}

// ---------------------------------------------------------------------------
// Per-route policy

/// A per-route rate limit, stored as route metadata and honored by a
/// [`RateLimit`] installed with [`App::wrap_route`](crate::App::wrap_route)
/// or [`Routes::wrap`].
///
/// Each route gets its own buckets, unless policies share a
/// [`scope`](Self::scope).
///
/// ```
/// use std::time::Duration;
/// use zapline::prelude::*;
/// use zapline::middleware::{RateLimit, RateLimitPolicy};
///
/// let app = App::new()
///     .post("/login", || async { "ok" })
///     .meta(RateLimitPolicy::new(5, Duration::from_secs(60)).burst(2).scope("auth"))
///     .get("/healthz", || async { "ok" })
///     .meta(RateLimitPolicy::exempt())
///     .wrap_route(RateLimit::per_ip(100, Duration::from_secs(60)));
/// ```
#[derive(Clone, Debug)]
pub struct RateLimitPolicy {
    quota: Option<Quota>,
    limit_header: HeaderValue,
    scope: Option<Arc<str>>,
}

impl RateLimitPolicy {
    /// `limit` requests per `period` (burst defaults to `limit`).
    ///
    /// # Panics
    /// If `limit` is 0 or `period` is zero.
    pub fn new(limit: u32, period: Duration) -> Self {
        RateLimitPolicy {
            quota: Some(Quota::new(limit, period)),
            limit_header: HeaderValue::from(limit),
            scope: None,
        }
    }

    /// Never limit this route.
    pub fn exempt() -> Self {
        RateLimitPolicy {
            quota: None,
            limit_header: HeaderValue::from_static("0"),
            scope: None,
        }
    }

    /// Allow up to `n` requests in a single burst.
    pub fn burst(mut self, n: u32) -> Self {
        self.quota = self.quota.map(|q| q.burst(n));
        self
    }

    /// Share buckets with every route whose policy has the same scope name.
    pub fn scope(mut self, name: &str) -> Self {
        self.scope = Some(name.into());
        self
    }

    /// The configured request limit (`None` when exempt).
    pub fn limit(&self) -> Option<u32> {
        self.quota.map(|q| q.limit)
    }
}

impl Routes {
    /// Rate-limit the most recently registered route to `limit` requests
    /// per `period`, per client. Needs a [`RateLimit`] installed with
    /// [`Routes::wrap`] / [`App::wrap_route`](crate::App::wrap_route),
    /// which decides how clients are identified.
    pub fn rate_limit(self, limit: u32, period: Duration) -> Self {
        self.meta(RateLimitPolicy::new(limit, period))
    }
}

impl App {
    /// Rate-limit the most recently registered route; see
    /// [`Routes::rate_limit`].
    pub fn rate_limit(self, limit: u32, period: Duration) -> Self {
        self.meta(RateLimitPolicy::new(limit, period))
    }
}

// ---------------------------------------------------------------------------
// Client IP resolution

/// An IP network such as `10.0.0.0/8` or `::1/128`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

/// Error parsing a [`Cidr`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CidrParseError(String);

impl fmt::Display for CidrParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid IP network {:?}", self.0)
    }
}

impl std::error::Error for CidrParseError {}

impl Cidr {
    /// A network from an address and prefix length. `None` when the prefix
    /// is longer than the address (32 bits for IPv4, 128 for IPv6).
    pub fn new(addr: IpAddr, prefix: u8) -> Option<Self> {
        let addr = addr.to_canonical();
        let max = if addr.is_ipv4() { 32 } else { 128 };
        (prefix <= max).then(|| Cidr {
            addr: mask(addr, prefix),
            prefix,
        })
    }

    /// Whether `ip` is inside this network.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        ip.is_ipv4() == self.addr.is_ipv4() && mask(ip, self.prefix) == self.addr
    }
}

fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let m = u32::MAX
                .checked_shl(32 - u32::from(prefix.min(32)))
                .unwrap_or(0);
            IpAddr::V4(Ipv4Addr::from(u32::from(v4) & m))
        }
        IpAddr::V6(v6) => {
            let m = u128::MAX
                .checked_shl(128 - u32::from(prefix.min(128)))
                .unwrap_or(0);
            IpAddr::V6(Ipv6Addr::from(u128::from(v6) & m))
        }
    }
}

impl From<IpAddr> for Cidr {
    fn from(ip: IpAddr) -> Self {
        let ip = ip.to_canonical();
        let prefix = if ip.is_ipv4() { 32 } else { 128 };
        Cidr { addr: ip, prefix }
    }
}

impl From<Ipv4Addr> for Cidr {
    fn from(ip: Ipv4Addr) -> Self {
        IpAddr::V4(ip).into()
    }
}

impl From<Ipv6Addr> for Cidr {
    fn from(ip: Ipv6Addr) -> Self {
        IpAddr::V6(ip).into()
    }
}

impl FromStr for Cidr {
    type Err = CidrParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || CidrParseError(s.to_owned());
        let s = s.trim();
        match s.split_once('/') {
            None => s.parse::<IpAddr>().map(Cidr::from).map_err(|_| err()),
            Some((ip, prefix)) => {
                let ip = ip.parse::<IpAddr>().map_err(|_| err())?;
                let prefix = prefix.parse::<u8>().map_err(|_| err())?;
                // A v4-mapped v6 network keeps its v4 meaning.
                let prefix = match ip {
                    IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() => {
                        prefix.checked_sub(96).ok_or_else(err)?
                    }
                    _ => prefix,
                };
                Cidr::new(ip, prefix).ok_or_else(err)
            }
        }
    }
}

impl TryFrom<&str> for Cidr {
    type Error = CidrParseError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl TryFrom<String> for Cidr {
    type Error = CidrParseError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// Which header trusted proxies use to report the client address.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClientIpHeader {
    /// `X-Forwarded-For: client, proxy1, proxy2` (default).
    XForwardedFor,
    /// RFC 7239 `Forwarded: for=client, for=proxy1`.
    Forwarded,
    /// A single-address header set by the proxy, e.g. `X-Real-IP`.
    Single(HeaderName),
}

/// Resolves the real client IP behind reverse proxies.
///
/// Forwarding headers are only honored when the TCP peer is a trusted
/// proxy. The header is then read right to left, skipping trusted proxies;
/// the first untrusted address is the client. Addresses a client wrote
/// itself (to the left) are never reached unless every hop is trusted.
///
/// ```
/// use zapline::middleware::rate_limit::{ClientIpHeader, TrustedProxies};
///
/// let proxies = TrustedProxies::new(["10.0.0.0/8", "127.0.0.1"])
///     .header(ClientIpHeader::Forwarded);
/// ```
#[derive(Clone, Debug, Default)]
pub struct TrustedProxies {
    nets: Arc<[Cidr]>,
    header: Option<ClientIpHeader>,
}

impl TrustedProxies {
    /// Trust the given proxies (IPs or CIDR strings).
    ///
    /// # Panics
    /// If an entry is not a valid IP address or network.
    pub fn new<I>(proxies: I) -> Self
    where
        I: IntoIterator,
        I::Item: TryInto<Cidr>,
        <I::Item as TryInto<Cidr>>::Error: fmt::Debug,
    {
        let nets: Vec<Cidr> = proxies
            .into_iter()
            .map(|p| {
                p.try_into()
                    .unwrap_or_else(|e| panic!("invalid trusted proxy: {e:?}"))
            })
            .collect();
        TrustedProxies {
            nets: nets.into(),
            header: None,
        }
    }

    /// Read the client address from this header (default `X-Forwarded-For`).
    pub fn header(mut self, header: ClientIpHeader) -> Self {
        self.header = Some(header);
        self
    }

    /// Whether `ip` is a trusted proxy.
    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|n| n.contains(ip))
    }

    /// The client address for `req`; `None` when the peer is unknown.
    pub fn client_ip(&self, req: &Request) -> Option<IpAddr> {
        let peer = req.remote_addr()?.ip().to_canonical();
        if self.nets.is_empty() || !self.is_trusted(peer) {
            return Some(peer);
        }
        let headers = req.headers();
        let mut last = peer;
        match self
            .header
            .as_ref()
            .unwrap_or(&ClientIpHeader::XForwardedFor)
        {
            ClientIpHeader::XForwardedFor => {
                for value in headers.get_all("x-forwarded-for").iter().rev() {
                    let Ok(value) = value.to_str() else {
                        return Some(last);
                    };
                    for hop in value.rsplit(',') {
                        match self.next_hop(parse_ip(hop), &mut last) {
                            Some(found) => return found,
                            None => continue,
                        }
                    }
                }
            }
            ClientIpHeader::Forwarded => {
                for value in headers.get_all(http::header::FORWARDED).iter().rev() {
                    let Ok(value) = value.to_str() else {
                        return Some(last);
                    };
                    for element in value.rsplit(',') {
                        match self.next_hop(forwarded_for(element), &mut last) {
                            Some(found) => return found,
                            None => continue,
                        }
                    }
                }
            }
            ClientIpHeader::Single(name) => {
                return Some(
                    headers
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_ip)
                        .unwrap_or(peer),
                );
            }
        }
        Some(last)
    }

    /// One step of the right-to-left walk: `Some(result)` ends the walk.
    #[inline]
    fn next_hop(&self, hop: Option<IpAddr>, last: &mut IpAddr) -> Option<Option<IpAddr>> {
        match hop {
            // Garbage in a header we trust: stop at the last good hop.
            None => Some(Some(*last)),
            Some(ip) if !self.is_trusted(ip) => Some(Some(ip)),
            Some(ip) => {
                *last = ip;
                None
            }
        }
    }
}

/// `1.2.3.4`, `1.2.3.4:80`, `::1`, `[::1]:80`, optionally quoted.
fn parse_ip(s: &str) -> Option<IpAddr> {
    let s = s.trim().trim_matches('"');
    if let Some(rest) = s.strip_prefix('[') {
        let end = rest.find(']')?;
        return rest[..end].parse::<Ipv6Addr>().ok().map(IpAddr::V6);
    }
    s.parse::<IpAddr>()
        .ok()
        .or_else(|| s.parse::<SocketAddr>().ok().map(|a| a.ip()))
        .map(|ip| ip.to_canonical())
}

/// The `for=` address of one `Forwarded` element.
fn forwarded_for(element: &str) -> Option<IpAddr> {
    element.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        k.trim().eq_ignore_ascii_case("for").then(|| parse_ip(v))?
    })
}

// ---------------------------------------------------------------------------
// Keys

/// Identifies who a request is accounted to. `None` skips limiting.
///
/// Implemented for closures `Fn(&Request) -> Option<String>`.
pub trait RateLimitKey: Send + Sync + 'static {
    /// The caller's key, or `None` to skip limiting this request.
    fn key(&self, req: &Request) -> Option<String>;
}

impl<F> RateLimitKey for F
where
    F: Fn(&Request) -> Option<String> + Send + Sync + 'static,
{
    fn key(&self, req: &Request) -> Option<String> {
        self(req)
    }
}

#[derive(Clone)]
enum KeyKind {
    Ip,
    Global,
    Header(HeaderName),
    Custom(Arc<dyn RateLimitKey>),
}

/// Bucket namespace: the middleware's own limit, a route, or a scope.
#[derive(Hash)]
enum Salt<'a> {
    Default,
    Route(usize),
    Scope(&'a str),
}

// ---------------------------------------------------------------------------
// Store

/// Keys are already well-mixed 64-bit hashes; don't hash them again.
#[derive(Default)]
struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | u64::from(b);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

struct Shard {
    /// key hash -> theoretical arrival time (ns since `Store::base`).
    map: HashMap<u64, u64, BuildHasherDefault<IdHasher>>,
    last_sweep: u64,
}

struct Store {
    shards: Box<[Mutex<Shard>]>,
    shard_cap: usize,
    hasher: RandomState,
    base: Instant,
    global: AtomicU64,
}

impl Store {
    fn new(max_keys: usize) -> Self {
        Store {
            shards: (0..SHARDS)
                .map(|_| {
                    Mutex::new(Shard {
                        map: HashMap::default(),
                        last_sweep: 0,
                    })
                })
                .collect(),
            shard_cap: max_keys.div_ceil(SHARDS).max(1),
            hasher: RandomState::new(),
            base: Instant::now(),
            global: AtomicU64::new(0),
        }
    }

    #[inline]
    fn now(&self) -> u64 {
        // +1 so a real timestamp is never 0 ("unknown").
        self.base.elapsed().as_nanos() as u64 + 1
    }

    fn check(&self, key: u64, quota: &Quota) -> Outcome {
        let now = self.now();
        // Middle bits pick the shard; the map uses the low and high bits.
        let shard = &self.shards[(key >> 32) as usize % SHARDS];
        let mut shard = shard.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(tat) = shard.map.get_mut(&key) {
            let (out, new) = quota.decide(*tat, now);
            if let Some(new) = new {
                *tat = new;
            }
            return out;
        }
        let (out, new) = quota.decide(0, now);
        if let Some(new) = new {
            shard.insert(key, new, now, self.shard_cap);
        }
        out
    }

    /// Lock-free path for the single global bucket.
    fn check_global(&self, quota: &Quota) -> Outcome {
        let now = self.now();
        let mut cur = self.global.load(Ordering::Acquire);
        loop {
            let (out, new) = quota.decide(cur, now);
            let Some(new) = new else { return out };
            match self
                .global
                .compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return out,
                Err(actual) => cur = actual,
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap_or_else(PoisonError::into_inner).map.len())
            .sum()
    }
}

impl Shard {
    fn insert(&mut self, key: u64, tat: u64, now: u64, cap: usize) {
        let full = self.map.len() >= cap;
        if full || now.saturating_sub(self.last_sweep) >= SWEEP_EVERY_NS {
            // A bucket whose TAT has passed is indistinguishable from a new one.
            self.map.retain(|_, t| *t > now);
            self.last_sweep = now;
        }
        if self.map.len() >= cap {
            // Still full: drop the bucket closest to expiry.
            if let Some(victim) = self.map.iter().min_by_key(|(_, t)| **t).map(|(k, _)| *k) {
                self.map.remove(&victim);
            }
        }
        self.map.insert(key, tat);
    }
}

// ---------------------------------------------------------------------------
// Middleware

type ExemptFn = Arc<dyn Fn(&Request) -> bool + Send + Sync>;

/// Rate-limiting middleware.
///
/// ```
/// use std::time::Duration;
/// use zapline::prelude::*;
/// use zapline::middleware::RateLimit;
///
/// let app = App::new().wrap(
///     RateLimit::per_ip(100, Duration::from_secs(60))
///         .burst(20)
///         .trust_forwarded_for(["10.0.0.0/8"])
///         .exempt(|req: &Request| req.path() == "/healthz"),
/// );
/// ```
///
/// Requests are spread evenly over the period (GCRA): `per_ip(100, 1 min)`
/// allows a burst of 100, then one request every 0.6 s. Memory is bounded
/// by [`max_keys`](Self::max_keys) (default 100 000 clients, ~2 MB);
/// expired entries are swept lazily, without a background task. Clones
/// share state.
#[derive(Clone)]
pub struct RateLimit {
    key: KeyKind,
    quota: Quota,
    limit_header: HeaderValue,
    headers: bool,
    exempt: Option<ExemptFn>,
    proxies: TrustedProxies,
    ipv6_prefix: u8,
    store: Arc<Store>,
}

impl RateLimit {
    fn with_key(key: KeyKind, limit: u32, period: Duration) -> Self {
        RateLimit {
            key,
            quota: Quota::new(limit, period),
            limit_header: HeaderValue::from(limit),
            headers: true,
            exempt: None,
            proxies: TrustedProxies::default(),
            ipv6_prefix: 64,
            store: Arc::new(Store::new(DEFAULT_MAX_KEYS)),
        }
    }

    /// `limit` requests per `period` for each client IP.
    ///
    /// # Panics
    /// If `limit` is 0 or `period` is zero (applies to every constructor).
    pub fn per_ip(limit: u32, period: Duration) -> Self {
        Self::with_key(KeyKind::Ip, limit, period)
    }

    /// `limit` requests per `period` shared by all clients.
    pub fn global(limit: u32, period: Duration) -> Self {
        Self::with_key(KeyKind::Global, limit, period)
    }

    /// `limit` requests per `period` per value of header `name` (e.g. an
    /// API key). Requests without the header are limited per client IP.
    ///
    /// # Panics
    /// If `name` is not a valid header name.
    pub fn by_header(name: &str, limit: u32, period: Duration) -> Self {
        let name = HeaderName::from_bytes(name.as_bytes())
            .unwrap_or_else(|_| panic!("invalid rate limit header name {name:?}"));
        Self::with_key(KeyKind::Header(name), limit, period)
    }

    /// `limit` requests per `period` per key returned by `key`; `None`
    /// skips limiting for that request.
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::prelude::*;
    /// use zapline::middleware::RateLimit;
    ///
    /// let per_tenant = RateLimit::by_key(1000, Duration::from_secs(60), |req: &Request| {
    ///     req.headers().get("x-tenant")?.to_str().ok().map(str::to_owned)
    /// });
    /// ```
    pub fn by_key<F>(limit: u32, period: Duration, key: F) -> Self
    where
        F: Fn(&Request) -> Option<String> + Send + Sync + 'static,
    {
        Self::keyed(limit, period, key)
    }

    /// Like [`by_key`](Self::by_key), with any [`RateLimitKey`].
    pub fn keyed(limit: u32, period: Duration, key: impl RateLimitKey) -> Self {
        Self::with_key(KeyKind::Custom(Arc::new(key)), limit, period)
    }

    /// Allow bursts of up to `n` requests (default: `limit`).
    pub fn burst(mut self, n: u32) -> Self {
        self.quota = self.quota.burst(n);
        self
    }

    /// Trust `X-Forwarded-For` from these proxies (IPs or CIDR strings such
    /// as `"10.0.0.0/8"`) to find the client IP. Without this the TCP peer
    /// address is used and forwarding headers are ignored.
    ///
    /// # Panics
    /// If an entry is not a valid IP address or network.
    pub fn trust_forwarded_for<I>(self, proxies: I) -> Self
    where
        I: IntoIterator,
        I::Item: TryInto<Cidr>,
        <I::Item as TryInto<Cidr>>::Error: fmt::Debug,
    {
        self.trusted_proxies(TrustedProxies::new(proxies))
    }

    /// Full control over client IP resolution (e.g. RFC 7239 `Forwarded`).
    pub fn trusted_proxies(mut self, proxies: TrustedProxies) -> Self {
        self.proxies = proxies;
        self
    }

    /// Group IPv6 clients by this prefix length (default 64: one bucket per
    /// /64, since one host usually controls a whole /64).
    pub fn ipv6_prefix(mut self, bits: u8) -> Self {
        self.ipv6_prefix = bits.min(128);
        self
    }

    /// Never limit requests for which `f` returns `true`.
    pub fn exempt<F>(mut self, f: F) -> Self
    where
        F: Fn(&Request) -> bool + Send + Sync + 'static,
    {
        self.exempt = Some(Arc::new(f));
        self
    }

    /// Whether to send `RateLimit-*` headers (default `true`). `429`
    /// responses always carry `Retry-After`.
    pub fn headers(mut self, enabled: bool) -> Self {
        self.headers = enabled;
        self
    }

    /// Maximum number of tracked clients (default 100 000). When full,
    /// the bucket closest to expiry is evicted.
    pub fn max_keys(mut self, n: usize) -> Self {
        self.store = Arc::new(Store::new(n));
        self
    }

    /// The client IP as this limiter sees it (honoring trusted proxies).
    pub fn client_ip(&self, req: &Request) -> Option<IpAddr> {
        self.proxies.client_ip(req)
    }

    /// Hash the request's key; `None` = don't limit.
    fn key_hash(&self, req: &Request, salt: &Salt<'_>) -> Option<u64> {
        let mut h = self.store.hasher.build_hasher();
        salt.hash(&mut h);
        match &self.key {
            KeyKind::Global => 0u8.hash(&mut h),
            KeyKind::Ip => self.hash_ip(req, &mut h),
            KeyKind::Header(name) => match req.headers().get(name) {
                Some(v) => {
                    2u8.hash(&mut h);
                    v.as_bytes().hash(&mut h);
                }
                None => self.hash_ip(req, &mut h),
            },
            KeyKind::Custom(k) => {
                3u8.hash(&mut h);
                k.key(req)?.hash(&mut h);
            }
        }
        Some(h.finish())
    }

    fn hash_ip(&self, req: &Request, h: &mut impl Hasher) {
        1u8.hash(h);
        // Unknown peers (e.g. Unix sockets) share one bucket.
        if let Some(ip) = self.client_ip(req) {
            match ip {
                IpAddr::V6(_) => mask(ip, self.ipv6_prefix).hash(h),
                v4 => v4.hash(h),
            }
        }
    }

    fn reject(&self, out: &Outcome, limit: &HeaderValue) -> Response {
        let retry = ceil_secs(out.retry_ns).max(1);
        let mut err = Error::too_many_requests()
            .with_code("rate_limited")
            .with_header(RETRY_AFTER, num_header(retry));
        if self.headers {
            err = err
                .with_header(RATELIMIT_LIMIT, limit.clone())
                .with_header(RATELIMIT_REMAINING, HeaderValue::from_static("0"))
                .with_header(RATELIMIT_RESET, num_header(ceil_secs(out.reset_ns)));
        }
        err.into_response()
    }
}

#[inline]
fn ceil_secs(ns: u64) -> u64 {
    ns.div_ceil(1_000_000_000)
}

/// A numeric header value; small numbers come from a table built once, so
/// the common case doesn't allocate.
fn num_header(n: u64) -> HeaderValue {
    const CACHED: usize = 1024;
    static TABLE: OnceLock<Vec<HeaderValue>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let digits: String = (0..CACHED).map(|i| i.to_string()).collect();
        let digits: &'static str = Box::leak(digits.into_boxed_str());
        let mut start = 0;
        (0..CACHED)
            .map(|i| {
                let end = start + i.to_string().len();
                let v = HeaderValue::from_static(&digits[start..end]);
                start = end;
                v
            })
            .collect()
    });
    usize::try_from(n)
        .ok()
        .and_then(|i| table.get(i))
        .cloned()
        .unwrap_or_else(|| HeaderValue::from(n))
}

impl Middleware for RateLimit {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        if self.exempt.as_ref().is_some_and(|f| f(&req)) {
            return next.run(req);
        }
        let route = req.route();
        let policy = route.and_then(|r| r.meta::<RateLimitPolicy>());
        let (quota, limit, salt) = match policy {
            Some(p) => match &p.quota {
                None => return next.run(req),
                Some(q) => (q, &p.limit_header, policy_salt(p, route)),
            },
            None => (&self.quota, &self.limit_header, Salt::Default),
        };
        let out = match (&self.key, &salt) {
            (KeyKind::Global, Salt::Default) => self.store.check_global(quota),
            _ => match self.key_hash(&req, &salt) {
                Some(key) => self.store.check(key, quota),
                None => return next.run(req),
            },
        };
        if !out.allowed {
            let res = self.reject(&out, limit);
            return Box::pin(std::future::ready(res));
        }
        let fut = next.run(req);
        if !self.headers {
            return fut;
        }
        let limit = limit.clone();
        let (remaining, reset) = (out.remaining, ceil_secs(out.reset_ns));
        Box::pin(async move {
            let mut res = fut.await;
            let h = res.headers_mut();
            // With nested limiters, report the most restrictive one.
            let tighter = h
                .get(&RATELIMIT_REMAINING)
                .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
                .is_none_or(|existing| remaining < existing);
            if tighter {
                h.insert(RATELIMIT_LIMIT, limit);
                h.insert(RATELIMIT_REMAINING, num_header(remaining));
                h.insert(RATELIMIT_RESET, num_header(reset));
            }
            res
        })
    }
}

fn policy_salt<'a>(p: &'a RateLimitPolicy, route: Option<&'static RouteInfo>) -> Salt<'a> {
    match (&p.scope, route) {
        (Some(scope), _) => Salt::Scope(scope),
        (None, Some(r)) => Salt::Route(r as *const RouteInfo as usize),
        (None, None) => Salt::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000_000;

    #[test]
    fn gcra_burst_then_steady() {
        let q = Quota::new(3, Duration::from_secs(3));
        let mut tat = 0;
        let now = 10 * SEC;
        for expected in [2, 1, 0] {
            let (out, new) = q.decide(tat, now);
            assert!(out.allowed);
            assert_eq!(out.remaining, expected);
            tat = new.unwrap();
        }
        let (out, new) = q.decide(tat, now);
        assert!(!out.allowed && new.is_none());
        assert_eq!(out.retry_ns, SEC);
        assert_eq!(out.reset_ns, 3 * SEC);
        // One interval later exactly one more request fits.
        let (out, new) = q.decide(tat, now + SEC);
        assert!(out.allowed);
        assert_eq!(out.remaining, 0);
        assert!(!q.decide(new.unwrap(), now + SEC).0.allowed);
    }

    #[test]
    fn burst_smaller_than_limit() {
        let q = Quota::new(10, Duration::from_secs(10)).burst(2);
        let now = SEC;
        let (a, t) = q.decide(0, now);
        let (b, t) = q.decide(t.unwrap(), now);
        let (c, _) = q.decide(t.unwrap(), now);
        assert!(a.allowed && b.allowed && !c.allowed);
        assert_eq!(a.remaining, 1);
    }

    #[test]
    fn store_is_bounded_and_sweeps() {
        let store = Store::new(SHARDS * 2);
        let q = Quota::new(1, Duration::from_secs(3600));
        for k in 0..10_000u64 {
            let key = k.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            assert!(store.check(key, &q).allowed);
        }
        assert!(store.len() <= SHARDS * 2, "len = {}", store.len());
    }

    #[test]
    fn cidr_parsing_and_matching() {
        let net: Cidr = "10.0.0.0/8".parse().unwrap();
        assert!(net.contains("10.1.2.3".parse().unwrap()));
        assert!(net.contains("::ffff:10.1.2.3".parse().unwrap()));
        assert!(!net.contains("11.0.0.1".parse().unwrap()));
        let v6: Cidr = "fd00::/8".parse().unwrap();
        assert!(v6.contains("fd12::1".parse().unwrap()));
        assert!(!v6.contains("10.0.0.1".parse().unwrap()));
        let host = Cidr::from(IpAddr::from([127, 0, 0, 1]));
        assert!(host.contains("127.0.0.1".parse().unwrap()));
        assert!(!host.contains("127.0.0.2".parse().unwrap()));
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains("8.8.8.8".parse().unwrap()));
        assert!("10.0.0.0/33".parse::<Cidr>().is_err());
        assert!("nope".parse::<Cidr>().is_err());
        assert!("::ffff:10.0.0.0/104"
            .parse::<Cidr>()
            .unwrap()
            .contains("10.9.9.9".parse().unwrap()));
    }

    #[test]
    fn numeric_header_values() {
        for n in [0, 7, 10, 99, 1023, 1024, 86_400, u64::MAX] {
            assert_eq!(num_header(n).to_str().unwrap(), n.to_string());
        }
    }

    #[test]
    fn ip_parsing() {
        assert_eq!(parse_ip(" 1.2.3.4 "), Some("1.2.3.4".parse().unwrap()));
        assert_eq!(parse_ip("1.2.3.4:80"), Some("1.2.3.4".parse().unwrap()));
        assert_eq!(parse_ip("\"[::1]:80\""), Some("::1".parse().unwrap()));
        assert_eq!(parse_ip("::1"), Some("::1".parse().unwrap()));
        assert_eq!(parse_ip("unknown"), None);
        assert_eq!(
            forwarded_for("proto=https;For=\"[2001:db8::1]:4711\""),
            Some("2001:db8::1".parse().unwrap())
        );
        assert_eq!(forwarded_for("by=1.2.3.4"), None);
    }
}
