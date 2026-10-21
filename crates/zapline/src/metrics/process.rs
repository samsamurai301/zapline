//! Standard `process_*` metrics, read at scrape time.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use super::registry::{Collector, Encoder, MetricKind, Num};

/// Process metrics in the format of the official Prometheus clients:
/// `process_start_time_seconds` everywhere, and on Linux also
/// `process_cpu_seconds_total`, `process_resident_memory_bytes`,
/// `process_virtual_memory_bytes`, `process_open_fds` and
/// `process_max_fds` (read from `/proc` at scrape time, so they cost nothing
/// between scrapes).
///
/// The [`Metrics`](super::Metrics) plugin includes it by default; add it to
/// your own [`Registry`](super::Registry) with `registry.register(ProcessCollector)`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCollector;

/// Clock ticks per second used by `/proc/<pid>/stat` (`USER_HZ`). It is 100
/// on every mainstream Linux architecture; the Go client hard-codes it too.
#[cfg(target_os = "linux")]
const USER_HZ: f64 = 100.0;

/// When this module was first used: the start-time fallback off Linux.
pub(crate) fn first_seen() -> f64 {
    static T: OnceLock<f64> = OnceLock::new();
    *T.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64())
    })
}

impl Collector for ProcessCollector {
    fn collect(&self, out: &mut Encoder<'_>) {
        let stats = linux::read();
        let start = stats
            .as_ref()
            .and_then(|s| s.start_time)
            .unwrap_or_else(first_seen);
        gauge(
            out,
            "process_start_time_seconds",
            "Start time of the process since unix epoch in seconds.",
            Num::F(start),
        );
        let Some(s) = stats else { return };
        if let Some(cpu) = s.cpu_seconds {
            out.header(
                "process_cpu_seconds_total",
                "Total user and system CPU time spent in seconds.",
                MetricKind::Counter,
            );
            out.write("process_cpu_seconds_total", "", &[], Num::F(cpu));
        }
        let gauges = [
            (
                "process_resident_memory_bytes",
                "Resident memory size in bytes.",
                s.rss_bytes,
            ),
            (
                "process_virtual_memory_bytes",
                "Virtual memory size in bytes.",
                s.vm_bytes,
            ),
            (
                "process_open_fds",
                "Number of open file descriptors.",
                s.open_fds,
            ),
            (
                "process_max_fds",
                "Maximum number of open file descriptors.",
                s.max_fds,
            ),
        ];
        for (name, help, v) in gauges {
            if let Some(v) = v {
                gauge(out, name, help, Num::U(v));
            }
        }
    }
}

fn gauge(out: &mut Encoder<'_>, name: &str, help: &str, v: Num) {
    out.header(name, help, MetricKind::Gauge);
    out.write(name, "", &[], v);
}

#[derive(Default)]
struct Stats {
    start_time: Option<f64>,
    cpu_seconds: Option<f64>,
    rss_bytes: Option<u64>,
    vm_bytes: Option<u64>,
    open_fds: Option<u64>,
    max_fds: Option<u64>,
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Stats, USER_HZ};
    use std::fs;

    pub(super) fn read() -> Option<Stats> {
        let stat = fs::read_to_string("/proc/self/stat").ok()?;
        let mut s = Stats::default();
        // Fields after the parenthesized command name (which may contain
        // spaces): state is field 3, utime 14, stime 15, starttime 22.
        let rest = &stat[stat.rfind(')')? + 1..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        let field = |n: usize| f.get(n - 3).and_then(|v| v.parse::<u64>().ok());
        if let (Some(u), Some(st)) = (field(14), field(15)) {
            s.cpu_seconds = Some((u + st) as f64 / USER_HZ);
        }
        if let (Some(start), Some(btime)) = (field(22), boot_time()) {
            s.start_time = Some(btime as f64 + start as f64 / USER_HZ);
        }
        if let Ok(status) = fs::read_to_string("/proc/self/status") {
            s.rss_bytes = kb_field(&status, "VmRSS:");
            s.vm_bytes = kb_field(&status, "VmSize:");
        }
        s.open_fds = fs::read_dir("/proc/self/fd").ok().map(|d| d.count() as u64);
        s.max_fds = fs::read_to_string("/proc/self/limits")
            .ok()
            .and_then(|l| max_open_files(&l));
        Some(s)
    }

    fn boot_time() -> Option<u64> {
        let stat = fs::read_to_string("/proc/stat").ok()?;
        stat.lines()
            .find_map(|l| l.strip_prefix("btime "))
            .and_then(|v| v.trim().parse().ok())
    }

    /// `VmRSS:     1234 kB` -> bytes.
    pub(super) fn kb_field(status: &str, key: &str) -> Option<u64> {
        let line = status.lines().find(|l| l.starts_with(key))?;
        let kb: u64 = line[key.len()..].split_whitespace().next()?.parse().ok()?;
        Some(kb * 1024)
    }

    /// The soft limit from `Max open files  1024  524288  files`.
    pub(super) fn max_open_files(limits: &str) -> Option<u64> {
        let line = limits.lines().find(|l| l.starts_with("Max open files"))?;
        line["Max open files".len()..]
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }
}

#[cfg(not(target_os = "linux"))]
mod linux {
    pub(super) fn read() -> Option<super::Stats> {
        None
    }
}
