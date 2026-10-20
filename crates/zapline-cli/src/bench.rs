//! `zapline bench`: a thin wrapper over [`wrk`](https://github.com/wg/wrk).

use std::io::ErrorKind;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::ui;

/// Validated benchmark settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchConfig {
    /// Target URL.
    pub url: String,
    /// Open connections.
    pub connections: usize,
    /// Worker threads of wrk.
    pub threads: usize,
    /// Duration in seconds.
    pub duration_secs: u64,
}

/// Parses `10`, `10s`, `2m` or `1h` into seconds. A bare number means seconds.
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    let (digits, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = digits
        .parse()
        .with_context(|| format!("invalid duration `{s}`; use e.g. 10s, 2m or 1h"))?;
    let secs = match unit {
        "s" => n,
        "m" => n.checked_mul(60).context("duration is too large")?,
        "h" => n.checked_mul(3600).context("duration is too large")?,
        _ => bail!("invalid duration unit in `{s}`; use s, m or h"),
    };
    if secs == 0 {
        bail!("duration must be at least 1s");
    }
    Ok(secs)
}

/// Validates the arguments and fills in the default thread count.
pub fn configure(
    url: &str,
    connections: usize,
    duration: &str,
    threads: Option<usize>,
    cpus: usize,
) -> Result<BenchConfig> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("`{url}` is not a valid URL; it must start with http:// or https://");
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("the URL must not contain whitespace");
    }
    if connections == 0 {
        bail!("--connections must be at least 1");
    }
    if threads == Some(0) {
        bail!("--threads must be at least 1");
    }
    // Leave some CPU for the server when benchmarking on one machine.
    let threads = threads.unwrap_or_else(|| cpus.clamp(1, 4));
    Ok(BenchConfig {
        url: url.to_string(),
        // wrk refuses more threads than connections.
        threads: threads.min(connections),
        connections,
        duration_secs: parse_duration(duration)?,
    })
}

impl BenchConfig {
    /// The arguments passed to `wrk`.
    pub fn wrk_args(&self) -> Vec<String> {
        vec![
            format!("-t{}", self.threads),
            format!("-c{}", self.connections),
            format!("-d{}s", self.duration_secs),
            "--latency".to_string(),
            self.url.clone(),
        ]
    }
}

/// The numbers extracted from wrk's report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WrkSummary {
    /// Requests per second.
    pub requests_per_sec: f64,
    /// Transfer rate such as `25.48MB`.
    pub transfer_per_sec: Option<String>,
    /// Mean latency such as `1.20ms`.
    pub latency_avg: Option<String>,
    /// Latency standard deviation.
    pub latency_stdev: Option<String>,
    /// Worst latency.
    pub latency_max: Option<String>,
    /// (percentile, latency), e.g. `("50%", "1.10ms")`.
    pub percentiles: Vec<(String, String)>,
    /// Total number of completed requests.
    pub total_requests: Option<u64>,
    /// Wall-clock time of the run as reported by wrk.
    pub total_time: Option<String>,
    /// Bytes read, such as `254.84MB`.
    pub data_read: Option<String>,
    /// wrk's `Socket errors:` summary.
    pub socket_errors: Option<String>,
    /// Number of non-2xx/3xx responses.
    pub non_2xx: Option<u64>,
}

/// Parses wrk's text report.
pub fn parse_wrk_output(text: &str) -> Result<WrkSummary> {
    let mut s = WrkSummary::default();
    let mut found_rate = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Requests/sec:") {
            if let Ok(v) = rest.trim().parse::<f64>() {
                s.requests_per_sec = v;
                found_rate = true;
            }
        } else if let Some(rest) = line.strip_prefix("Transfer/sec:") {
            s.transfer_per_sec = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("Latency ") {
            // "Latency   1.20ms  512.30us  25.12ms   88.41%" (not "Latency Distribution")
            let cols: Vec<&str> = rest.split_whitespace().collect();
            if cols.len() >= 3 && cols[0] != "Distribution" {
                s.latency_avg = Some(cols[0].to_string());
                s.latency_stdev = Some(cols[1].to_string());
                s.latency_max = Some(cols[2].to_string());
            }
        } else if let Some(rest) = line.strip_prefix("Socket errors:") {
            s.socket_errors = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("Non-2xx or 3xx responses:") {
            s.non_2xx = rest.trim().parse().ok();
        } else if let Some((pct, value)) = percentile_line(line) {
            s.percentiles.push((pct, value));
        } else if let Some((n, time, read)) = totals_line(line) {
            s.total_requests = Some(n);
            s.total_time = Some(time);
            s.data_read = Some(read);
        }
    }
    if !found_rate {
        bail!("could not find `Requests/sec` in wrk's output");
    }
    Ok(s)
}

/// `99%    3.20ms` -> ("99%", "3.20ms")
fn percentile_line(line: &str) -> Option<(String, String)> {
    let mut cols = line.split_whitespace();
    let pct = cols.next()?;
    let value = cols.next()?;
    let digits = pct.strip_suffix('%')?;
    if digits.is_empty()
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        || cols.next().is_some()
    {
        return None;
    }
    Some((pct.to_string(), value.to_string()))
}

/// `2087654 requests in 10.00s, 254.84MB read` -> (2087654, "10.00s", "254.84MB")
fn totals_line(line: &str) -> Option<(u64, String, String)> {
    let (count, rest) = line.split_once(" requests in ")?;
    let n = count.trim().parse().ok()?;
    let (time, read) = rest.split_once(',')?;
    let read = read.trim().strip_suffix(" read").unwrap_or(read.trim());
    Some((n, time.trim().to_string(), read.to_string()))
}

/// Formats a float with thousands separators: `1234567.891` -> `1,234,567.89`.
pub fn format_rate(v: f64) -> String {
    let fixed = format!("{v:.2}");
    let (int, frac) = fixed.split_once('.').unwrap_or((&fixed, "00"));
    let digits: Vec<char> = int.chars().collect();
    let mut grouped = String::new();
    for (i, c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*c);
    }
    format!("{grouped}.{frac}")
}

/// Renders the human-friendly summary.
pub fn render_summary(cfg: &BenchConfig, s: &WrkSummary, color: bool) -> String {
    let mut out = String::new();
    out.push_str(&ui::bold(
        color,
        format!(
            "{}  ({} connections, {} threads, {}s)\n",
            cfg.url, cfg.connections, cfg.threads, cfg.duration_secs
        ),
    ));
    out.push_str(&format!(
        "  {:<14}{}\n",
        "Requests/sec",
        ui::bold(color, format_rate(s.requests_per_sec))
    ));
    if let Some(t) = &s.transfer_per_sec {
        out.push_str(&format!("  {:<14}{t}\n", "Transfer/sec"));
    }
    if let (Some(avg), Some(sd), Some(max)) = (&s.latency_avg, &s.latency_stdev, &s.latency_max) {
        out.push_str(&format!(
            "  {:<14}avg {avg}  stdev {sd}  max {max}\n",
            "Latency"
        ));
    }
    if !s.percentiles.is_empty() {
        let parts: Vec<String> = s
            .percentiles
            .iter()
            .map(|(p, v)| format!("p{} {v}", p.trim_end_matches('%')))
            .collect();
        out.push_str(&format!("  {:<14}{}\n", "Percentiles", parts.join("  ")));
    }
    if let (Some(n), Some(t), Some(read)) = (s.total_requests, &s.total_time, &s.data_read) {
        out.push_str(&format!(
            "  {:<14}{n} requests in {t}, {read} read\n",
            "Total"
        ));
    }
    let errors = s.socket_errors.as_deref();
    let non_2xx = s.non_2xx.unwrap_or(0);
    if errors.is_some() || non_2xx > 0 {
        let mut parts = Vec::new();
        if let Some(e) = errors {
            parts.push(format!("socket errors: {e}"));
        }
        if non_2xx > 0 {
            parts.push(format!("non-2xx/3xx responses: {non_2xx}"));
        }
        out.push_str(&format!(
            "  {:<14}{}\n",
            "Errors",
            ui::paint(color, "\x1b[33m", parts.join("; "))
        ));
    }
    out
}

/// Runs wrk and prints the summary (or wrk's raw output with `raw`).
pub fn run(cfg: &BenchConfig, raw: bool) -> Result<()> {
    ui::status(
        "Benchmarking",
        format!(
            "{} for {}s with {} connections",
            cfg.url, cfg.duration_secs, cfg.connections
        ),
    );
    let output = match Command::new("wrk").args(cfg.wrk_args()).output() {
        Ok(o) => o,
        Err(e) if e.kind() == ErrorKind::NotFound => bail!(
            "`wrk` was not found on your PATH\n\
             hint: install it (`apt install wrk`, `brew install wrk`, or build https://github.com/wg/wrk), \
             then run `zapline doctor` to verify"
        ),
        Err(e) => return Err(e).context("failed to run wrk"),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "wrk failed ({}): {}",
            output.status,
            stderr.trim().lines().last().unwrap_or("no output")
        );
    }
    if raw {
        print!("{stdout}");
        return Ok(());
    }
    let summary =
        parse_wrk_output(&stdout).with_context(|| format!("unexpected wrk output:\n{stdout}"))?;
    print!("{}", render_summary(cfg, &summary, ui::stdout_color()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRK: &str = "\
Running 10s test @ http://127.0.0.1:8080/
  4 threads and 256 connections
  Thread Stats   Avg      Stdev     Max   +/- Stdev
    Latency     1.20ms  512.30us  25.12ms   88.41%
    Req/Sec    52.31k     4.11k   61.20k    73.00%
  Latency Distribution
     50%    1.10ms
     75%    1.30ms
     90%    1.80ms
     99%    3.20ms
  2087654 requests in 10.00s, 254.84MB read
  Socket errors: connect 0, read 12, write 0, timeout 0
  Non-2xx or 3xx responses: 5
Requests/sec: 208765.43
Transfer/sec:     25.48MB
";

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("10s").unwrap(), 10);
        assert_eq!(parse_duration("10").unwrap(), 10);
        assert_eq!(parse_duration("2m").unwrap(), 120);
        assert_eq!(parse_duration("1h").unwrap(), 3600);
        assert_eq!(parse_duration(" 30s ").unwrap(), 30);
        for bad in [
            "",
            "s",
            "0s",
            "-5s",
            "10x",
            "1.5s",
            "10 s",
            "abc",
            "99999999999999999999s",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn configure_validates_and_defaults() {
        let cfg = configure("http://localhost:8080/", 256, "10s", None, 16).unwrap();
        assert_eq!(cfg.threads, 4);
        assert_eq!(cfg.duration_secs, 10);
        assert_eq!(
            configure("http://x", 256, "5s", None, 2).unwrap().threads,
            2
        );
        assert_eq!(
            configure("http://x", 256, "5s", None, 0).unwrap().threads,
            1
        );
        assert_eq!(
            configure("http://x", 256, "5s", Some(8), 2)
                .unwrap()
                .threads,
            8
        );
        // Never more threads than connections.
        assert_eq!(
            configure("http://x", 2, "5s", Some(8), 2).unwrap().threads,
            2
        );
        assert!(configure("localhost:8080", 1, "5s", None, 1).is_err());
        assert!(configure("ftp://x", 1, "5s", None, 1).is_err());
        assert!(configure("http://x y", 1, "5s", None, 1).is_err());
        assert!(configure("http://x", 0, "5s", None, 1).is_err());
        assert!(configure("http://x", 1, "5s", Some(0), 1).is_err());
        assert!(configure("http://x", 1, "bogus", None, 1).is_err());
        assert!(configure("https://example.com/path?q=1", 1, "5s", None, 1).is_ok());
    }

    #[test]
    fn builds_wrk_arguments() {
        let cfg = configure("http://localhost:8080/", 128, "30s", Some(2), 8).unwrap();
        assert_eq!(
            cfg.wrk_args(),
            [
                "-t2",
                "-c128",
                "-d30s",
                "--latency",
                "http://localhost:8080/"
            ]
        );
    }

    #[test]
    fn parses_wrk_output() {
        let s = parse_wrk_output(WRK).unwrap();
        assert_eq!(s.requests_per_sec, 208765.43);
        assert_eq!(s.transfer_per_sec.as_deref(), Some("25.48MB"));
        assert_eq!(s.latency_avg.as_deref(), Some("1.20ms"));
        assert_eq!(s.latency_stdev.as_deref(), Some("512.30us"));
        assert_eq!(s.latency_max.as_deref(), Some("25.12ms"));
        assert_eq!(
            s.percentiles,
            [
                ("50%", "1.10ms"),
                ("75%", "1.30ms"),
                ("90%", "1.80ms"),
                ("99%", "3.20ms")
            ]
            .map(|(a, b)| (a.to_string(), b.to_string()))
        );
        assert_eq!(s.total_requests, Some(2087654));
        assert_eq!(s.total_time.as_deref(), Some("10.00s"));
        assert_eq!(s.data_read.as_deref(), Some("254.84MB"));
        assert_eq!(
            s.socket_errors.as_deref(),
            Some("connect 0, read 12, write 0, timeout 0")
        );
        assert_eq!(s.non_2xx, Some(5));
    }

    #[test]
    fn minimal_wrk_output_is_enough() {
        let s = parse_wrk_output("Requests/sec: 12.5\n").unwrap();
        assert_eq!(s.requests_per_sec, 12.5);
        assert!(s.percentiles.is_empty());
        assert!(parse_wrk_output("nothing useful").is_err());
        assert!(parse_wrk_output("").is_err());
    }

    #[test]
    fn formats_rates() {
        assert_eq!(format_rate(208765.43), "208,765.43");
        assert_eq!(format_rate(0.0), "0.00");
        assert_eq!(format_rate(999.999), "1,000.00");
        assert_eq!(format_rate(1234567.891), "1,234,567.89");
        assert_eq!(format_rate(12.5), "12.50");
        assert_eq!(format_rate(100000.0), "100,000.00");
    }

    #[test]
    fn renders_a_summary() {
        let cfg = configure("http://127.0.0.1:8080/", 256, "10s", Some(4), 4).unwrap();
        let text = render_summary(&cfg, &parse_wrk_output(WRK).unwrap(), false);
        assert!(
            text.starts_with("http://127.0.0.1:8080/  (256 connections, 4 threads, 10s)\n"),
            "{text}"
        );
        assert!(text.contains("  Requests/sec  208,765.43\n"), "{text}");
        assert!(text.contains("  Transfer/sec  25.48MB\n"), "{text}");
        assert!(
            text.contains("  Latency       avg 1.20ms  stdev 512.30us  max 25.12ms\n"),
            "{text}"
        );
        assert!(
            text.contains("p50 1.10ms  p75 1.30ms  p90 1.80ms  p99 3.20ms"),
            "{text}"
        );
        assert!(
            text.contains("2087654 requests in 10.00s, 254.84MB read"),
            "{text}"
        );
        assert!(
            text.contains(
                "socket errors: connect 0, read 12, write 0, timeout 0; non-2xx/3xx responses: 5"
            ),
            "{text}"
        );
    }

    #[test]
    fn clean_runs_report_no_errors_line() {
        let cfg = configure("http://x", 1, "1s", Some(1), 1).unwrap();
        let text = render_summary(&cfg, &parse_wrk_output("Requests/sec: 5\n").unwrap(), false);
        assert!(!text.contains("Errors"), "{text}");
    }
}
