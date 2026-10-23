//! Cron expressions: parse once, then ask for the next fire time.
//!
//! Used by [`Schedule::cron`](crate::tasks::Schedule::cron) for in-process
//! scheduled tasks and, with a database, by
//! [`JobQueue::cron`](crate::jobs::JobQueue::cron) for durable jobs that run
//! once per tick across every instance of the app.
//!
//! ```
//! use std::time::{Duration, UNIX_EPOCH};
//! use zapline::cron::Cron;
//!
//! // 09:30 on weekdays, UTC.
//! let cron = Cron::parse("30 9 * * MON-FRI")?;
//! let friday_noon = UNIX_EPOCH + Duration::from_secs(1_704_456_000); // 2024-01-05 12:00 UTC
//! let next = cron.next_after(friday_noon).unwrap();
//! assert_eq!(next, UNIX_EPOCH + Duration::from_secs(1_704_706_200)); // Monday 2024-01-08 09:30
//! # Ok::<(), zapline::cron::CronError>(())
//! ```
//!
//! # Syntax
//!
//! Five fields (`minute hour day-of-month month day-of-week`), or six with
//! seconds first (`second minute hour day-of-month month day-of-week`):
//!
//! | Field | Values | Names |
//! |---|---|---|
//! | second | 0-59 | |
//! | minute | 0-59 | |
//! | hour | 0-23 | |
//! | day of month | 1-31 | |
//! | month | 1-12 | `JAN`-`DEC` |
//! | day of week | 0-7 (0 and 7 are Sunday) | `SUN`-`SAT` |
//!
//! Each field is a comma-separated list of `*`, a value `5`, a range `1-5`,
//! or a step `*/15`, `10-50/20`, `5/10` (from 5 to the end). Ranges may wrap
//! around (`FRI-MON`, `22-2` for hours). Names are case-insensitive.
//!
//! Extensions, in the day fields:
//!
//! | Token | Field | Meaning |
//! |---|---|---|
//! | `?` | either day field | same as `*` |
//! | `L` | day of month | the last day of the month |
//! | `L-3` | day of month | three days before the last day |
//! | `15W` | day of month | the weekday (Monday to Friday) nearest the 15th, within the month |
//! | `LW` | day of month | the last weekday of the month |
//! | `FRI#2` | day of week | the second Friday of the month (`#1` to `#5`) |
//! | `5L` | day of week | the last Friday of the month |
//!
//! When both day fields are restricted (neither starts with `*` or `?`), a
//! day matches if **either** matches, as in classic cron: `0 0 1 * MON`
//! runs on the 1st and on every Monday. Otherwise both must match.
//!
//! Macros: `@yearly` (or `@annually`), `@monthly`, `@weekly`, `@daily` (or
//! `@midnight`) and `@hourly`.
//!
//! # Time zones
//!
//! Expressions run in UTC unless they start with `CRON_TZ=<zone>` (or
//! `TZ=<zone>`), or [`Cron::with_timezone`] sets one. A zone is `UTC`, a
//! fixed offset such as `+05:30`, or, with the `tz` Cargo feature, an IANA
//! name such as `Europe/Paris` (read from the system's time zone database,
//! bundled on Windows).
//!
//! Around daylight-saving changes:
//!
//! - When clocks go forward, a time that doesn't exist that day (02:30 in a
//!   skipped hour) fires once, at the moment the clocks jump.
//! - When clocks go back, a time that happens twice fires once, the first
//!   time. A schedule whose hour field covers every hour (`*/15 * * * *`)
//!   keeps its rhythm through both passes instead.

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A parsed cron expression with its time zone. See the
/// [module documentation](self) for the syntax.
///
/// ```
/// use std::time::SystemTime;
/// use zapline::cron::Cron;
///
/// let cron: Cron = "CRON_TZ=+02:00 0 0 9 * * MON#1".parse()?; // first Monday, 09:00 UTC+2
/// let next_three: Vec<SystemTime> = cron.upcoming(SystemTime::now()).take(3).collect();
/// assert_eq!(next_three.len(), 3);
/// assert_eq!(cron.to_string(), "CRON_TZ=+02:00 0 0 9 * * MON#1");
/// # Ok::<(), zapline::cron::CronError>(())
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Cron {
    seconds: u64,
    minutes: u64,
    hours: u32,
    /// Bits 1..=12.
    months: u16,
    days: DaySpec,
    tz: Tz,
    /// The expression as written, without the time zone prefix.
    expr: Box<str>,
}

/// What the two day fields select.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct DaySpec {
    /// Day of month, bits 1..=31.
    dom: u32,
    /// `L-n`: bit `n` set means `n` days before the last day (`L` is bit 0).
    dom_last: u32,
    /// `nW`: bits 1..=31.
    dom_nearest: u32,
    /// `LW`.
    dom_last_weekday: bool,
    /// Day of week, bits 0..=6 (Sunday is 0).
    dow: u8,
    /// `d#n`: for each weekday, bits 1..=5.
    dow_nth: [u8; 7],
    /// `dL`: bits 0..=6.
    dow_last: u8,
    /// The field starts with `*` or `?`.
    dom_any: bool,
    dow_any: bool,
}

/// Why a cron expression or a time zone was rejected.
///
/// ```
/// use zapline::cron::Cron;
///
/// let err = Cron::parse("0 25 * * *").unwrap_err();
/// assert_eq!(err.to_string(), "invalid cron expression `0 25 * * *`: hour `25` is out of range 0-23");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronError {
    input: String,
    message: String,
}

impl CronError {
    fn new(input: &str, message: impl Into<String>) -> Self {
        CronError {
            input: input.to_owned(),
            message: message.into(),
        }
    }

    /// The reason alone, without the expression.
    ///
    /// ```
    /// let err = zapline::cron::Cron::parse("* * *").unwrap_err();
    /// assert!(err.reason().starts_with("expected 5 fields"));
    /// ```
    pub fn reason(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid cron expression `{}`: {}",
            self.input, self.message
        )
    }
}

impl std::error::Error for CronError {}

// ---------------------------------------------------------------------------
// Time zones

/// The time zone a [`Cron`] runs in: UTC (the default), a fixed offset, or,
/// with the `tz` feature, an IANA zone.
///
/// ```
/// use zapline::cron::Tz;
///
/// assert_eq!(Tz::parse("utc")?, Tz::UTC);
/// assert_eq!(Tz::parse("+05:30")?, Tz::fixed(5 * 3600 + 30 * 60)?);
/// assert_eq!(Tz::parse("UTC-8")?.to_string(), "-08:00");
/// # Ok::<(), zapline::cron::CronError>(())
/// ```
#[derive(Clone)]
pub struct Tz(TzKind);

#[derive(Clone)]
enum TzKind {
    Fixed(i32),
    #[cfg(feature = "tz")]
    Named(jiff::tz::TimeZone, Box<str>),
}

/// Offsets must stay within a day.
const MAX_OFFSET: i32 = 24 * 3600 - 1;

impl Tz {
    /// Coordinated Universal Time.
    pub const UTC: Tz = Tz(TzKind::Fixed(0));

    /// A fixed offset east of UTC, in seconds (`-8 * 3600` for UTC-8).
    ///
    /// ```
    /// let tz = zapline::cron::Tz::fixed(-8 * 3600)?;
    /// assert_eq!(tz.to_string(), "-08:00");
    /// # Ok::<(), zapline::cron::CronError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// If the offset is a day or more.
    pub fn fixed(offset_seconds: i32) -> Result<Tz, CronError> {
        if offset_seconds.abs() > MAX_OFFSET {
            return Err(CronError::new(
                &offset_seconds.to_string(),
                "a UTC offset must be less than 24 hours",
            ));
        }
        Ok(Tz(TzKind::Fixed(offset_seconds)))
    }

    /// `UTC` (also `Z`, `GMT`, `Etc/UTC`), an offset (`+02:00`, `-0530`,
    /// `+9`, `UTC+2`, `GMT-03:30`), or, with the `tz` feature, an IANA name
    /// (`Europe/Paris`, `America/New_York`).
    ///
    /// ```
    /// use zapline::cron::Tz;
    ///
    /// assert_eq!(Tz::parse("GMT+01:00")?.to_string(), "+01:00");
    /// # #[cfg(feature = "tz")]
    /// assert_eq!(Tz::parse("Europe/Paris")?.to_string(), "Europe/Paris");
    /// # Ok::<(), zapline::cron::CronError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// If the zone is unknown, or is an IANA name without the `tz` feature.
    pub fn parse(name: &str) -> Result<Tz, CronError> {
        let name = name.trim();
        let upper = name.to_ascii_uppercase();
        if matches!(upper.as_str(), "UTC" | "Z" | "GMT" | "ETC/UTC" | "UCT") {
            return Ok(Tz::UTC);
        }
        let offset = upper
            .strip_prefix("UTC")
            .or_else(|| upper.strip_prefix("GMT"))
            .unwrap_or(&upper);
        if offset.starts_with(['+', '-']) {
            return parse_offset(offset)
                .ok_or_else(|| {
                    CronError::new(name, "a UTC offset looks like `+02:00`, `-0530` or `+9`")
                })
                .and_then(Tz::fixed)
                .map_err(|e| CronError::new(name, e.message));
        }
        named_zone(name)
    }

    /// The offset from UTC, in seconds, at `t` (seconds since the epoch).
    fn offset_at(&self, t: i64) -> i64 {
        match &self.0 {
            TzKind::Fixed(off) => {
                let _ = t;
                i64::from(*off)
            }
            #[cfg(feature = "tz")]
            TzKind::Named(tz, _) => {
                jiff_ts(t).map_or(0, |ts| i64::from(tz.to_offset(ts).seconds()))
            }
        }
    }

    /// The first offset change strictly after `t`.
    fn next_transition(&self, t: i64) -> Option<i64> {
        match &self.0 {
            TzKind::Fixed(_) => {
                let _ = t;
                None
            }
            #[cfg(feature = "tz")]
            TzKind::Named(tz, _) => tz
                .following(jiff_ts(t)?)
                .map(|tr| tr.timestamp().as_second())
                .find(|&at| at > t),
        }
    }

    /// The last offset change at or before `t`, with the offset in force
    /// just before it.
    fn last_transition(&self, t: i64) -> Option<(i64, i64)> {
        match &self.0 {
            TzKind::Fixed(_) => {
                let _ = t;
                None
            }
            #[cfg(feature = "tz")]
            TzKind::Named(tz, _) => {
                let at = tz
                    .preceding(jiff_ts(t.checked_add(1)?)?)
                    .map(|tr| tr.timestamp().as_second())
                    .find(|&at| at <= t)?;
                Some((at, self.offset_at(at - 1)))
            }
        }
    }
}

#[cfg(feature = "tz")]
fn jiff_ts(t: i64) -> Option<jiff::Timestamp> {
    jiff::Timestamp::from_second(t).ok()
}

#[cfg(feature = "tz")]
fn named_zone(name: &str) -> Result<Tz, CronError> {
    match jiff::tz::TimeZone::get(name) {
        Ok(tz) => Ok(Tz(TzKind::Named(tz, name.into()))),
        Err(e) => Err(CronError::new(name, format!("unknown time zone: {e}"))),
    }
}

#[cfg(not(feature = "tz"))]
fn named_zone(name: &str) -> Result<Tz, CronError> {
    Err(CronError::new(
        name,
        "named time zones need the `tz` Cargo feature (UTC and offsets such as `+02:00` work without it)",
    ))
}

/// `+HH`, `+H`, `+HHMM`, `+HH:MM`, `+HH:MM:SS`, as seconds east of UTC.
fn parse_offset(s: &str) -> Option<i32> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let parts: Vec<&str> = if rest.contains(':') {
        rest.split(':').collect()
    } else if rest.len() > 2 {
        if rest.len() != 4 {
            return None;
        }
        vec![&rest[..2], &rest[2..]]
    } else {
        vec![rest]
    };
    if parts.is_empty()
        || parts.len() > 3
        || parts
            .iter()
            .any(|p| p.is_empty() || p.len() > 2 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let num = |i: usize| parts.get(i).map_or(Some(0), |p| p.parse::<i32>().ok());
    let (h, m, sec) = (num(0)?, num(1)?, num(2)?);
    if m >= 60 || sec >= 60 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60 + sec))
}

impl fmt::Display for Tz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            TzKind::Fixed(0) => f.write_str("UTC"),
            TzKind::Fixed(off) => {
                let sign = if *off < 0 { '-' } else { '+' };
                let a = off.unsigned_abs();
                write!(f, "{sign}{:02}:{:02}", a / 3600, a / 60 % 60)?;
                if a % 60 != 0 {
                    write!(f, ":{:02}", a % 60)?;
                }
                Ok(())
            }
            #[cfg(feature = "tz")]
            TzKind::Named(_, name) => f.write_str(name),
        }
    }
}

impl fmt::Debug for Tz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tz({self})")
    }
}

impl PartialEq for Tz {
    fn eq(&self, other: &Tz) -> bool {
        match (&self.0, &other.0) {
            (TzKind::Fixed(a), TzKind::Fixed(b)) => a == b,
            #[cfg(feature = "tz")]
            (TzKind::Named(_, a), TzKind::Named(_, b)) => a == b,
            #[cfg(feature = "tz")]
            _ => false,
        }
    }
}

impl Eq for Tz {}

impl Default for Tz {
    fn default() -> Self {
        Tz::UTC
    }
}

// ---------------------------------------------------------------------------
// Parsing

/// Something [`Schedule::cron`](crate::tasks::Schedule::cron) and
/// [`JobQueue::cron`](crate::jobs::JobQueue::cron) accept: an expression
/// (`&str`, `String`) or an already parsed [`Cron`].
///
/// ```
/// use zapline::cron::{Cron, IntoCron};
///
/// assert!("@hourly".into_cron().is_ok());
/// assert!(Cron::parse("@daily").unwrap().into_cron().is_ok());
/// ```
pub trait IntoCron {
    /// Parse (or pass through) the expression.
    ///
    /// # Errors
    ///
    /// If the expression is invalid.
    fn into_cron(self) -> Result<Cron, CronError>;
}

impl IntoCron for Cron {
    fn into_cron(self) -> Result<Cron, CronError> {
        Ok(self)
    }
}

impl IntoCron for &Cron {
    fn into_cron(self) -> Result<Cron, CronError> {
        Ok(self.clone())
    }
}

impl IntoCron for &str {
    fn into_cron(self) -> Result<Cron, CronError> {
        Cron::parse(self)
    }
}

impl IntoCron for String {
    fn into_cron(self) -> Result<Cron, CronError> {
        Cron::parse(&self)
    }
}

impl IntoCron for &String {
    fn into_cron(self) -> Result<Cron, CronError> {
        Cron::parse(self)
    }
}

impl FromStr for Cron {
    type Err = CronError;

    fn from_str(s: &str) -> Result<Cron, CronError> {
        Cron::parse(s)
    }
}

const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// One numeric field's bounds and names.
#[derive(Clone, Copy)]
struct Field {
    name: &'static str,
    lo: u32,
    hi: u32,
    names: &'static [&'static str],
}

const SECOND: Field = Field {
    name: "second",
    lo: 0,
    hi: 59,
    names: &[],
};
const MINUTE: Field = Field {
    name: "minute",
    lo: 0,
    hi: 59,
    names: &[],
};
const HOUR: Field = Field {
    name: "hour",
    lo: 0,
    hi: 23,
    names: &[],
};
const DOM: Field = Field {
    name: "day of month",
    lo: 1,
    hi: 31,
    names: &[],
};
const MONTH: Field = Field {
    name: "month",
    lo: 1,
    hi: 12,
    names: &MONTHS,
};
/// 7 is Sunday too; folded into bit 0 after parsing.
const DOW: Field = Field {
    name: "day of week",
    lo: 0,
    hi: 7,
    names: &WEEKDAYS,
};

impl Field {
    fn value(&self, s: &str) -> Result<u32, String> {
        if let Some(i) = self.names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Ok(self.lo + i as u32);
        }
        let v: u32 = s.parse().map_err(|_| {
            if self.names.is_empty() {
                format!("{} `{s}` is not a number", self.name)
            } else {
                format!(
                    "{} `{s}` is not a number or a name ({}-{})",
                    self.name,
                    self.names[0],
                    self.names[self.names.len() - 1]
                )
            }
        })?;
        if v < self.lo || v > self.hi {
            return Err(format!(
                "{} `{s}` is out of range {}-{}",
                self.name, self.lo, self.hi
            ));
        }
        Ok(v)
    }

    /// A comma-separated list of `*`, `a`, `a-b`, with optional `/step`.
    fn parse(&self, text: &str) -> Result<u64, String> {
        let mut mask = 0u64;
        for part in text.split(',') {
            mask |= self.parse_part(part)?;
        }
        Ok(mask)
    }

    fn parse_part(&self, part: &str) -> Result<u64, String> {
        if part.is_empty() {
            return Err(format!("empty item in the {} field", self.name));
        }
        // In the day-of-week field `*` and `MON/2` stop at Saturday (7 is
        // only an alias for Sunday).
        let weekdays = self.names == WEEKDAYS;
        let hi = if weekdays { 6 } else { self.hi };
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let span = hi - self.lo + 1;
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|s| (1..=span).contains(s))
                    .ok_or_else(|| {
                        format!(
                            "step `{step}` in the {} field must be a number from 1 to {span}",
                            self.name
                        )
                    })?;
                (range, Some(step))
            }
            None => (part, None),
        };
        let (a, b) = if range == "*" || range == "?" {
            (self.lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (self.value(a)?, self.value(b)?)
        } else {
            let a = self.value(range)?;
            (a, if step.is_some() { hi.max(a) } else { a })
        };
        let step = step.unwrap_or(1) as usize;
        let mut mask = 0u64;
        if a <= b {
            for v in (a..=b).step_by(step) {
                mask |= 1 << v;
            }
        } else {
            // A range that wraps around: FRI-MON, 22-2.
            let a = if weekdays && a == 7 { 0 } else { a };
            for v in (a..=hi).chain(self.lo..=b).step_by(step) {
                mask |= 1 << v;
            }
        }
        Ok(mask)
    }
}

fn fields_of<'a>(input: &str, body: &'a str) -> Result<Vec<&'a str>, CronError> {
    let mut fields: Vec<&str> = if let Some(name) = body.strip_prefix('@') {
        let expanded = match name.to_ascii_lowercase().as_str() {
            "yearly" | "annually" => "0 0 0 1 1 *",
            "monthly" => "0 0 0 1 * *",
            "weekly" => "0 0 0 * * 0",
            "daily" | "midnight" => "0 0 0 * * *",
            "hourly" => "0 0 * * * *",
            _ => {
                return Err(CronError::new(
                    input,
                    format!(
                        "unknown macro `{body}` (use @yearly, @annually, @monthly, @weekly, @daily, @midnight or @hourly)"
                    ),
                ))
            }
        };
        return Ok(expanded.split(' ').collect());
    } else {
        body.split_whitespace().collect()
    };
    match fields.len() {
        5 => fields.insert(0, "0"),
        6 => {}
        n => {
            return Err(CronError::new(
                input,
                format!(
                    "expected 5 fields (minute hour day-of-month month day-of-week) \
                     or 6 (second first), found {n}"
                ),
            ))
        }
    }
    Ok(fields)
}

impl Cron {
    /// Parse an expression. See the [module documentation](self).
    ///
    /// ```
    /// use zapline::cron::Cron;
    ///
    /// assert!(Cron::parse("*/5 9-17 * * MON-FRI").is_ok());
    /// assert!(Cron::parse("0 0 L * *").is_ok());     // last day of every month
    /// assert!(Cron::parse("0 0 30 2 *").is_err());   // February 30th never happens
    /// ```
    ///
    /// # Errors
    ///
    /// On a syntax error, an out-of-range value, an unknown time zone, or an
    /// expression that never matches a real date.
    pub fn parse(input: &str) -> Result<Cron, CronError> {
        let trimmed = input.trim();
        let (tz, body) = split_tz(trimmed).map_err(|e| CronError::new(input, e.message))?;
        let fields = fields_of(input, body)?;
        let err = |m: String| CronError::new(input, m);
        let seconds = SECOND.parse(fields[0]).map_err(err)?;
        let minutes = MINUTE.parse(fields[1]).map_err(err)?;
        let hours = HOUR.parse(fields[2]).map_err(err)? as u32;
        let months = MONTH.parse(fields[4]).map_err(err)? as u16;
        let mut days = parse_dom(fields[3]).map_err(err)?;
        parse_dow(fields[5], &mut days).map_err(err)?;
        let cron = Cron {
            seconds,
            minutes,
            hours,
            months,
            days,
            tz,
            expr: body.split_whitespace().collect::<Vec<_>>().join(" ").into(),
        };
        if cron.next_local(-1).is_none() {
            return Err(CronError::new(
                input,
                "it never matches a real date (like February 30th)",
            ));
        }
        Ok(cron)
    }

    /// The same schedule in another time zone.
    ///
    /// ```
    /// use zapline::cron::{Cron, Tz};
    ///
    /// let cron = Cron::parse("0 9 * * *")?.with_timezone(Tz::parse("+01:00")?);
    /// assert_eq!(cron.to_string(), "CRON_TZ=+01:00 0 9 * * *");
    /// # Ok::<(), zapline::cron::CronError>(())
    /// ```
    pub fn with_timezone(mut self, tz: Tz) -> Cron {
        self.tz = tz;
        self
    }

    /// The time zone the expression runs in.
    ///
    /// ```
    /// use zapline::cron::{Cron, Tz};
    ///
    /// assert_eq!(*Cron::parse("@daily")?.timezone(), Tz::UTC);
    /// # Ok::<(), zapline::cron::CronError>(())
    /// ```
    pub fn timezone(&self) -> &Tz {
        &self.tz
    }

    /// The first fire time strictly after `t`, or `None` if there is none
    /// before the year 10000 or so. Fire times fall on whole seconds.
    ///
    /// ```
    /// use std::time::{Duration, UNIX_EPOCH};
    /// use zapline::cron::Cron;
    ///
    /// let every_15 = Cron::parse("*/15 * * * *")?;
    /// let t = UNIX_EPOCH + Duration::from_secs(60); // 00:01
    /// assert_eq!(every_15.next_after(t), Some(UNIX_EPOCH + Duration::from_secs(15 * 60)));
    /// # Ok::<(), zapline::cron::CronError>(())
    /// ```
    pub fn next_after(&self, t: SystemTime) -> Option<SystemTime> {
        let secs = match t.duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_secs()).ok()?,
            Err(e) => {
                let d = e.duration();
                let whole = i64::try_from(d.as_secs()).ok()?;
                -whole - i64::from(d.subsec_nanos() > 0)
            }
        };
        let next = self.next_secs(secs)?;
        if next >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(next as u64))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(next.unsigned_abs()))
        }
    }

    /// Every fire time after `t`, in order.
    ///
    /// ```
    /// use std::time::{Duration, UNIX_EPOCH};
    /// use zapline::cron::Cron;
    ///
    /// let hourly = Cron::parse("@hourly")?;
    /// let runs: Vec<_> = hourly.upcoming(UNIX_EPOCH).take(2).collect();
    /// assert_eq!(runs, [UNIX_EPOCH + Duration::from_secs(3600), UNIX_EPOCH + Duration::from_secs(7200)]);
    /// # Ok::<(), zapline::cron::CronError>(())
    /// ```
    pub fn upcoming(&self, t: SystemTime) -> impl Iterator<Item = SystemTime> + '_ {
        let mut at = t;
        std::iter::from_fn(move || {
            at = self.next_after(at)?;
            Some(at)
        })
    }

    /// Whether the hour field covers every hour (a repeated hour then runs
    /// twice).
    fn every_hour(&self) -> bool {
        self.hours == (1 << 24) - 1
    }

    /// The first fire instant (seconds since the epoch) strictly after
    /// `after`, applying the time zone and its daylight-saving rules.
    fn next_secs(&self, mut after: i64) -> Option<i64> {
        // Each pass handles one stretch of constant UTC offset; the bound
        // only guards against a broken time zone database.
        for _ in 0..100_000 {
            let start = after.checked_add(1)?;
            let off = self.tz.offset_at(start);
            let end = self.tz.next_transition(start);
            let local = self.next_local(after.checked_add(off)?)?;
            let at = local - off;
            if end.is_none_or(|end| at < end) {
                // Clocks went back: this wall time already happened under the
                // previous offset, so it already fired.
                if !self.every_hour() {
                    if let Some((changed, before)) = self.tz.last_transition(at) {
                        if before > off && local < changed + before {
                            after = at;
                            continue;
                        }
                    }
                }
                return Some(at);
            }
            let end = end?;
            let next_off = self.tz.offset_at(end);
            if next_off > off && local < end + next_off {
                // Clocks jump forward over this wall time: fire at the jump.
                return Some(end);
            }
            after = end - 1;
        }
        None
    }

    /// The first matching wall-clock time strictly after `after` (seconds
    /// since 1970-01-01 00:00 local), searching up to 400 years ahead (a
    /// whole Gregorian cycle, so every day/month/weekday combination is
    /// seen).
    fn next_local(&self, after: i64) -> Option<i64> {
        let t = after.checked_add(1)?;
        let (mut y, mut mo, mut d) = civil_from_days(t.div_euclid(DAY));
        let sod = t.rem_euclid(DAY);
        let (mut h, mut mi, mut s) = (
            (sod / 3600) as u32,
            (sod / 60 % 60) as u32,
            (sod % 60) as u32,
        );
        let last_year = y + 400;
        // Move to the start of the next day.
        macro_rules! next_day {
            () => {{
                d += 1;
                if d > days_in_month(y, mo) {
                    d = 1;
                    mo += 1;
                    if mo > 12 {
                        mo = 1;
                        y += 1;
                    }
                }
                h = 0;
                mi = 0;
                s = 0;
                continue;
            }};
        }
        loop {
            if y > last_year {
                return None;
            }
            if self.months & (1 << mo) == 0 {
                match next_bit(u64::from(self.months), mo + 1) {
                    Some(m) => mo = m,
                    None => {
                        y += 1;
                        mo = self.months.trailing_zeros();
                    }
                }
                d = 1;
                h = 0;
                mi = 0;
                s = 0;
                continue;
            }
            if !self.days.matches(y, mo, d) {
                next_day!();
            }
            match next_bit(u64::from(self.hours), h) {
                None => next_day!(),
                Some(nh) if nh != h => {
                    h = nh;
                    mi = 0;
                    s = 0;
                }
                Some(_) => {}
            }
            match next_bit(self.minutes, mi) {
                None => {
                    h += 1;
                    mi = 0;
                    s = 0;
                    if h == 24 {
                        next_day!();
                    }
                    continue;
                }
                Some(nm) if nm != mi => {
                    mi = nm;
                    s = 0;
                }
                Some(_) => {}
            }
            match next_bit(self.seconds, s) {
                None => {
                    mi += 1;
                    s = 0;
                    if mi == 60 {
                        mi = 0;
                        h += 1;
                        if h == 24 {
                            next_day!();
                        }
                    }
                    continue;
                }
                Some(ns) => s = ns,
            }
            return Some(
                days_from_civil(y, mo, d) * DAY
                    + i64::from(h) * 3600
                    + i64::from(mi) * 60
                    + i64::from(s),
            );
        }
    }
}

/// Split a leading `CRON_TZ=zone` / `TZ=zone`.
fn split_tz(s: &str) -> Result<(Tz, &str), CronError> {
    for prefix in ["CRON_TZ=", "TZ="] {
        if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
            let rest = &s[prefix.len()..];
            let (zone, body) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            return Ok((Tz::parse(zone)?, body.trim_start()));
        }
    }
    Ok((Tz::UTC, s))
}

fn parse_dom(text: &str) -> Result<DaySpec, String> {
    let mut spec = DaySpec {
        dom: 0,
        dom_last: 0,
        dom_nearest: 0,
        dom_last_weekday: false,
        dow: 0,
        dow_nth: [0; 7],
        dow_last: 0,
        dom_any: text.starts_with(['*', '?']),
        dow_any: false,
    };
    for part in text.split(',') {
        let upper = part.to_ascii_uppercase();
        if upper == "L" {
            spec.dom_last |= 1;
        } else if upper == "LW" || upper == "WL" {
            spec.dom_last_weekday = true;
        } else if let Some(n) = upper.strip_prefix("L-") {
            let n: u32 = n
                .parse()
                .ok()
                .filter(|n| *n <= 30)
                .ok_or_else(|| format!("`{part}` must be `L-n` with n from 0 to 30"))?;
            spec.dom_last |= 1 << n;
        } else if let Some(n) = upper.strip_suffix('W') {
            let n = DOM.value(n)?;
            spec.dom_nearest |= 1 << n;
        } else {
            spec.dom |= DOM.parse_part(part)? as u32;
        }
    }
    Ok(spec)
}

fn parse_dow(text: &str, spec: &mut DaySpec) -> Result<(), String> {
    spec.dow_any = text.starts_with(['*', '?']);
    let weekday = |s: &str| DOW.value(s).map(|d| (d % 7) as usize);
    for part in text.split(',') {
        if let Some((day, n)) = part.split_once('#') {
            let day = weekday(day)?;
            let n: u8 = n
                .parse()
                .ok()
                .filter(|n| (1..=5).contains(n))
                .ok_or_else(|| format!("`{part}`: the week number after `#` must be 1 to 5"))?;
            spec.dow_nth[day] |= 1 << n;
        } else if part.len() > 1 && part.ends_with(['L', 'l']) {
            let day = weekday(&part[..part.len() - 1])?;
            spec.dow_last |= 1 << day;
        } else if part.eq_ignore_ascii_case("L") {
            return Err("`L` alone is ambiguous in the day-of-week field: use `SATL`, `5L`... for the last such day of the month".into());
        } else {
            let mask = DOW.parse_part(part)?;
            // 7 is Sunday.
            spec.dow |= ((mask | (mask >> 7)) & 0x7f) as u8;
        }
    }
    Ok(())
}

impl DaySpec {
    fn matches(&self, y: i64, m: u32, d: u32) -> bool {
        let dim = days_in_month(y, m);
        let wd = weekday(y, m, d);
        let dom = self.dom & (1 << d) != 0
            || (self.dom_last != 0 && dim - d < 31 && self.dom_last & (1 << (dim - d)) != 0)
            || (self.dom_nearest != 0 && self.nearest_weekday(y, m, d, dim))
            || (self.dom_last_weekday && d == last_weekday(y, m, dim));
        let dow = self.dow & (1 << wd) != 0
            || self.dow_nth[wd as usize] & (1 << ((d - 1) / 7 + 1)) != 0
            || (self.dow_last & (1 << wd) != 0 && d + 7 > dim);
        if self.dom_any || self.dow_any {
            dom && dow
        } else {
            dom || dow
        }
    }

    /// Whether `d` is the weekday nearest to one of the `nW` days.
    fn nearest_weekday(&self, y: i64, m: u32, d: u32, dim: u32) -> bool {
        let mut bits = self.dom_nearest;
        while bits != 0 {
            let n = bits.trailing_zeros();
            bits &= bits - 1;
            if n > dim {
                continue;
            }
            let target = match weekday(y, m, n) {
                6 if n == 1 => 3,       // Saturday the 1st: Monday the 3rd
                6 => n - 1,             // Saturday: Friday
                0 if n == dim => n - 2, // Sunday the last: Friday
                0 => n + 1,             // Sunday: Monday
                _ => n,
            };
            if target == d {
                return true;
            }
        }
        false
    }
}

fn last_weekday(y: i64, m: u32, dim: u32) -> u32 {
    match weekday(y, m, dim) {
        6 => dim - 1,
        0 => dim - 2,
        _ => dim,
    }
}

/// The lowest set bit of `mask` at or above `from`.
fn next_bit(mask: u64, from: u32) -> Option<u32> {
    if from >= 64 {
        return None;
    }
    let m = mask & (u64::MAX << from);
    (m != 0).then(|| m.trailing_zeros())
}

impl fmt::Display for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.tz != Tz::UTC {
            write!(f, "CRON_TZ={} ", self.tz)?;
        }
        f.write_str(&self.expr)
    }
}

impl fmt::Debug for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cron({self})")
    }
}

// ---------------------------------------------------------------------------
// Calendar arithmetic (proleptic Gregorian, after Howard Hinnant)

const DAY: i64 = 86_400;

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// 0 is Sunday.
fn weekday(y: i64, m: u32, d: u32) -> u32 {
    (days_from_civil(y, m, d) + 4).rem_euclid(7) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `YYYY-MM-DD HH:MM:SS` as seconds since the epoch, UTC.
    fn ts(s: &str) -> i64 {
        let n = |r: std::ops::Range<usize>| s[r].parse::<i64>().unwrap();
        days_from_civil(n(0..4), n(5..7) as u32, n(8..10) as u32) * DAY
            + n(11..13) * 3600
            + n(14..16) * 60
            + n(17..19)
    }

    fn fmt(t: i64) -> String {
        let (y, m, d) = civil_from_days(t.div_euclid(DAY));
        let s = t.rem_euclid(DAY);
        format!(
            "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
            s / 3600,
            s / 60 % 60,
            s % 60
        )
    }

    /// The next `n` fire times after `from`, formatted.
    fn runs(expr: &str, from: &str, n: usize) -> Vec<String> {
        let cron = Cron::parse(expr).unwrap_or_else(|e| panic!("{e}"));
        let mut at = ts(from);
        (0..n)
            .map(|_| {
                at = cron.next_secs(at).expect("a next run");
                fmt(at)
            })
            .collect()
    }

    fn next(expr: &str, from: &str) -> String {
        runs(expr, from, 1).remove(0)
    }

    #[test]
    fn calendar_round_trips() {
        for z in -800_000..800_000 {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
            assert!((1..=days_in_month(y, m)).contains(&d));
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(weekday(1970, 1, 1), 4);
        assert_eq!(weekday(2024, 2, 29), 4);
        assert_eq!(weekday(2000, 1, 1), 6);
        assert!(is_leap(2000) && is_leap(2024) && !is_leap(1900) && !is_leap(2100));
    }

    #[test]
    fn five_and_six_fields() {
        assert_eq!(
            next("* * * * *", "2024-01-01 00:00:00"),
            "2024-01-01 00:01:00"
        );
        assert_eq!(
            next("* * * * *", "2024-01-01 00:00:59"),
            "2024-01-01 00:01:00"
        );
        assert_eq!(
            next("* * * * * *", "2024-01-01 00:00:00"),
            "2024-01-01 00:00:01"
        );
        assert_eq!(
            next("30 * * * * *", "2024-01-01 00:00:30"),
            "2024-01-01 00:01:30"
        );
        assert_eq!(
            next("0 0 * * *", "2024-01-01 00:00:00"),
            "2024-01-02 00:00:00"
        );
        assert_eq!(
            next("15 10 * * *", "2024-01-01 10:14:59"),
            "2024-01-01 10:15:00"
        );
        assert_eq!(
            next("15 10 * * *", "2024-01-01 10:15:00"),
            "2024-01-02 10:15:00"
        );
        assert_eq!(
            runs("*/20 * * * * *", "2024-01-01 23:59:30", 3),
            [
                "2024-01-01 23:59:40",
                "2024-01-02 00:00:00",
                "2024-01-02 00:00:20"
            ]
        );
    }

    #[test]
    fn ranges_steps_lists() {
        assert_eq!(
            runs("0 9-11 * * *", "2024-01-01 00:00:00", 4),
            [
                "2024-01-01 09:00:00",
                "2024-01-01 10:00:00",
                "2024-01-01 11:00:00",
                "2024-01-02 09:00:00"
            ]
        );
        assert_eq!(
            runs("*/15 * * * *", "2024-01-01 00:50:00", 3),
            [
                "2024-01-01 01:00:00",
                "2024-01-01 01:15:00",
                "2024-01-01 01:30:00"
            ]
        );
        assert_eq!(
            runs("10-50/20 * * * *", "2024-01-01 00:00:00", 4),
            [
                "2024-01-01 00:10:00",
                "2024-01-01 00:30:00",
                "2024-01-01 00:50:00",
                "2024-01-01 01:10:00"
            ]
        );
        assert_eq!(
            runs("5/25 * * * *", "2024-01-01 00:00:00", 3),
            [
                "2024-01-01 00:05:00",
                "2024-01-01 00:30:00",
                "2024-01-01 00:55:00"
            ]
        );
        assert_eq!(
            runs("0 0,12 1,15 * *", "2024-01-01 00:00:00", 4),
            [
                "2024-01-01 12:00:00",
                "2024-01-15 00:00:00",
                "2024-01-15 12:00:00",
                "2024-02-01 00:00:00"
            ]
        );
        assert_eq!(
            runs("0 1,3-4,*/12 * * *", "2024-01-01 00:00:00", 5),
            [
                "2024-01-01 01:00:00",
                "2024-01-01 03:00:00",
                "2024-01-01 04:00:00",
                "2024-01-01 12:00:00",
                "2024-01-02 00:00:00"
            ]
        );
        // Wrapping hour range.
        assert_eq!(
            runs("0 22-1 * * *", "2024-01-01 12:00:00", 4),
            [
                "2024-01-01 22:00:00",
                "2024-01-01 23:00:00",
                "2024-01-02 00:00:00",
                "2024-01-02 01:00:00"
            ]
        );
    }

    #[test]
    fn names_and_weekdays() {
        // 2024-01-01 is a Monday.
        assert_eq!(
            next("0 9 * * MON-FRI", "2024-01-05 12:00:00"),
            "2024-01-08 09:00:00"
        );
        assert_eq!(
            next("0 9 * * sat,sun", "2024-01-01 00:00:00"),
            "2024-01-06 09:00:00"
        );
        assert_eq!(
            next("0 9 * * 0", "2024-01-01 00:00:00"),
            "2024-01-07 09:00:00"
        );
        assert_eq!(
            next("0 9 * * 7", "2024-01-01 00:00:00"),
            "2024-01-07 09:00:00"
        );
        assert_eq!(
            next("0 9 * * 5-7", "2024-01-01 00:00:00"),
            "2024-01-05 09:00:00"
        );
        assert_eq!(
            runs("0 9 * * FRI-MON", "2024-01-02 00:00:00", 4),
            [
                "2024-01-05 09:00:00",
                "2024-01-06 09:00:00",
                "2024-01-07 09:00:00",
                "2024-01-08 09:00:00"
            ]
        );
        assert_eq!(
            next("0 0 1 JAN,jul *", "2024-02-01 00:00:00"),
            "2024-07-01 00:00:00"
        );
        assert_eq!(
            next("0 0 1 NOV-FEB *", "2024-03-01 00:00:00"),
            "2024-11-01 00:00:00"
        );
        assert_eq!(
            runs("0 0 1 NOV-FEB *", "2024-12-15 00:00:00", 3),
            [
                "2025-01-01 00:00:00",
                "2025-02-01 00:00:00",
                "2025-11-01 00:00:00"
            ]
        );
        assert_eq!(
            next("0 0 * * MON/2", "2024-01-01 00:00:00"),
            "2024-01-03 00:00:00"
        );
    }

    #[test]
    fn dom_and_dow_combine_like_vixie_cron() {
        // Both restricted: either matches. 2024-01-01 is a Monday.
        assert_eq!(
            runs("0 0 13 * FRI", "2024-01-01 00:00:00", 4),
            [
                "2024-01-05 00:00:00",
                "2024-01-12 00:00:00",
                "2024-01-13 00:00:00",
                "2024-01-19 00:00:00"
            ]
        );
        // A `*`-prefixed field means both must match.
        assert_eq!(
            next("0 0 */2 * MON", "2024-01-01 00:00:00"),
            "2024-01-15 00:00:00"
        );
        assert_eq!(
            next("0 0 ? * MON", "2024-01-01 00:00:00"),
            "2024-01-08 00:00:00"
        );
        assert_eq!(
            next("0 0 13 * ?", "2024-01-01 00:00:00"),
            "2024-01-13 00:00:00"
        );
    }

    #[test]
    fn month_ends() {
        assert_eq!(
            runs("0 0 31 * *", "2024-01-31 00:00:00", 4),
            [
                "2024-03-31 00:00:00",
                "2024-05-31 00:00:00",
                "2024-07-31 00:00:00",
                "2024-08-31 00:00:00"
            ]
        );
        assert_eq!(
            runs("0 0 30 * *", "2024-01-30 00:00:00", 2),
            ["2024-03-30 00:00:00", "2024-04-30 00:00:00"]
        );
        assert_eq!(
            runs("0 0 L * *", "2024-01-01 00:00:00", 4),
            [
                "2024-01-31 00:00:00",
                "2024-02-29 00:00:00",
                "2024-03-31 00:00:00",
                "2024-04-30 00:00:00"
            ]
        );
        assert_eq!(
            next("0 0 L * *", "2023-02-01 00:00:00"),
            "2023-02-28 00:00:00"
        );
        assert_eq!(
            next("0 0 L-1 * *", "2023-02-01 00:00:00"),
            "2023-02-27 00:00:00"
        );
        assert_eq!(
            next("0 0 L-2 2 *", "2024-01-01 00:00:00"),
            "2024-02-27 00:00:00"
        );
        assert_eq!(
            next("0 0 1,L * *", "2024-01-01 00:00:00"),
            "2024-01-31 00:00:00"
        );
        // Last weekday: 2024-03-31 is a Sunday, 2024-08-31 a Saturday.
        assert_eq!(
            next("0 0 LW * *", "2024-03-01 00:00:00"),
            "2024-03-29 00:00:00"
        );
        assert_eq!(
            next("0 0 LW * *", "2024-08-01 00:00:00"),
            "2024-08-30 00:00:00"
        );
        assert_eq!(
            next("0 0 LW * *", "2024-07-01 00:00:00"),
            "2024-07-31 00:00:00"
        );
        // Year rollover.
        assert_eq!(
            next("59 23 31 12 *", "2024-06-01 00:00:00"),
            "2024-12-31 23:59:00"
        );
        assert_eq!(
            next("0 0 1 1 *", "2024-12-31 23:59:59"),
            "2025-01-01 00:00:00"
        );
        assert_eq!(
            next("* * * * * *", "2024-12-31 23:59:59"),
            "2025-01-01 00:00:00"
        );
    }

    #[test]
    fn leap_years() {
        assert_eq!(
            runs("0 0 29 2 *", "2023-01-01 00:00:00", 3),
            [
                "2024-02-29 00:00:00",
                "2028-02-29 00:00:00",
                "2032-02-29 00:00:00"
            ]
        );
        // 2100 is not a leap year; 2000 was.
        assert_eq!(
            next("0 0 29 2 *", "2096-03-01 00:00:00"),
            "2104-02-29 00:00:00"
        );
        assert_eq!(
            next("0 0 29 2 *", "1999-01-01 00:00:00"),
            "2000-02-29 00:00:00"
        );
        // A fifth Monday in February: only when the 29th is a Monday.
        assert_eq!(
            next("0 0 * 2 MON#5", "2024-03-01 00:00:00"),
            "2044-02-29 00:00:00"
        );
        assert_eq!(
            next("0 0 L 2 *", "2100-01-01 00:00:00"),
            "2100-02-28 00:00:00"
        );
    }

    #[test]
    fn nth_and_last_weekday() {
        // Second Friday: 2024-01-12, 2024-02-09.
        assert_eq!(
            runs("0 9 * * FRI#2", "2024-01-01 00:00:00", 2),
            ["2024-01-12 09:00:00", "2024-02-09 09:00:00"]
        );
        assert_eq!(
            next("0 9 * * 1#1", "2024-01-02 00:00:00"),
            "2024-02-05 09:00:00"
        );
        // Last Friday: 2024-01-26, 2024-02-23, 2024-03-29.
        assert_eq!(
            runs("0 0 * * 5L", "2024-01-01 00:00:00", 3),
            [
                "2024-01-26 00:00:00",
                "2024-02-23 00:00:00",
                "2024-03-29 00:00:00"
            ]
        );
        assert_eq!(
            next("0 0 * * SUNL", "2024-03-01 00:00:00"),
            "2024-03-31 00:00:00"
        );
        // Fifth Monday only in months that have one: 2024-01-29, 2024-04-29.
        assert_eq!(
            runs("0 0 * * MON#5", "2024-01-01 00:00:00", 2),
            ["2024-01-29 00:00:00", "2024-04-29 00:00:00"]
        );
    }

    #[test]
    fn nearest_weekday() {
        // 2024-06-15 is a Saturday: Friday the 14th.
        assert_eq!(
            next("0 0 15W * *", "2024-06-01 00:00:00"),
            "2024-06-14 00:00:00"
        );
        // 2024-09-15 is a Sunday: Monday the 16th.
        assert_eq!(
            next("0 0 15W * *", "2024-09-01 00:00:00"),
            "2024-09-16 00:00:00"
        );
        // 2024-06-01 is a Saturday: not May 31st but Monday June 3rd.
        assert_eq!(
            next("0 0 1W * *", "2024-05-02 00:00:00"),
            "2024-06-03 00:00:00"
        );
        // 2024-03-31 is a Sunday: Friday the 29th, not April 1st.
        assert_eq!(
            next("0 0 31W * *", "2024-03-01 00:00:00"),
            "2024-03-29 00:00:00"
        );
        // 31W skips months without a 31st.
        assert_eq!(
            next("0 0 31W * *", "2024-04-01 00:00:00"),
            "2024-05-31 00:00:00"
        );
        // A weekday stays put.
        assert_eq!(
            next("0 0 10W * *", "2024-01-01 00:00:00"),
            "2024-01-10 00:00:00"
        );
    }

    #[test]
    fn macros() {
        let from = "2024-05-15 10:20:30";
        assert_eq!(next("@yearly", from), "2025-01-01 00:00:00");
        assert_eq!(next("@annually", from), "2025-01-01 00:00:00");
        assert_eq!(next("@monthly", from), "2024-06-01 00:00:00");
        assert_eq!(next("@weekly", from), "2024-05-19 00:00:00");
        assert_eq!(next("@daily", from), "2024-05-16 00:00:00");
        assert_eq!(next("@midnight", from), "2024-05-16 00:00:00");
        assert_eq!(next("@hourly", from), "2024-05-15 11:00:00");
        assert_eq!(next("@HOURLY", from), "2024-05-15 11:00:00");
    }

    #[test]
    fn fixed_offsets() {
        // 09:00 at UTC+02:00 is 07:00 UTC.
        assert_eq!(
            next("CRON_TZ=+02:00 0 9 * * *", "2024-01-01 08:00:00"),
            "2024-01-02 07:00:00"
        );
        assert_eq!(
            next("TZ=UTC+2 0 9 * * *", "2024-01-01 06:00:00"),
            "2024-01-01 07:00:00"
        );
        // 23:30 at UTC-05:00 is 04:30 UTC the next day.
        assert_eq!(
            next("CRON_TZ=-05:00 30 23 * * *", "2024-01-01 00:00:00"),
            "2024-01-01 04:30:00"
        );
        // The day of week is the local one: Monday 01:00 at +05:30 is Sunday 19:30 UTC.
        assert_eq!(
            next("CRON_TZ=+0530 0 1 * * MON", "2024-01-01 00:00:00"),
            "2024-01-07 19:30:00"
        );
        assert_eq!(
            next("cron_tz=utc @daily", "2024-01-01 00:00:00"),
            "2024-01-02 00:00:00"
        );
        let cron = Cron::parse("CRON_TZ=-08:00  0  9 * * *").unwrap();
        assert_eq!(cron.to_string(), "CRON_TZ=-08:00 0 9 * * *");
        assert_eq!(Cron::parse(&cron.to_string()).unwrap(), cron);
        assert_eq!(
            Cron::parse("CRON_TZ=UTC 0 9 * * *").unwrap().to_string(),
            "0 9 * * *"
        );
    }

    #[test]
    fn offsets_parse() {
        assert_eq!(parse_offset("+02:00"), Some(7200));
        assert_eq!(parse_offset("-0530"), Some(-19800));
        assert_eq!(parse_offset("+9"), Some(32400));
        assert_eq!(parse_offset("+05:30:15"), Some(19815));
        assert_eq!(parse_offset("+5:3"), Some(18180));
        assert_eq!(parse_offset("+123"), None);
        assert_eq!(parse_offset("+02:60"), None);
        assert_eq!(parse_offset("+aa"), None);
        assert_eq!(parse_offset("+"), None);
        assert!(Tz::parse("+24:00").is_err());
        assert!(Tz::fixed(86_400).is_err());
        assert_eq!(Tz::fixed(-86_399).unwrap().to_string(), "-23:59:59");
        assert_eq!(Tz::parse("Z").unwrap(), Tz::UTC);
        assert_eq!(Tz::parse("GMT-03:30").unwrap().to_string(), "-03:30");
    }

    #[test]
    fn errors() {
        let reason = |e: &str| Cron::parse(e).unwrap_err().reason().to_owned();
        assert_eq!(reason("60 * * * *"), "minute `60` is out of range 0-59");
        assert_eq!(reason("* 24 * * *"), "hour `24` is out of range 0-23");
        assert_eq!(reason("* * 0 * *"), "day of month `0` is out of range 1-31");
        assert_eq!(reason("* * * 13 *"), "month `13` is out of range 1-12");
        assert_eq!(reason("* * * * 8"), "day of week `8` is out of range 0-7");
        assert_eq!(
            reason("* * * FOO *"),
            "month `FOO` is not a number or a name (JAN-DEC)"
        );
        assert_eq!(reason("x * * * *"), "minute `x` is not a number");
        assert_eq!(
            reason("*/0 * * * *"),
            "step `0` in the minute field must be a number from 1 to 60"
        );
        assert_eq!(reason("1,,2 * * * *"), "empty item in the minute field");
        assert!(reason("* * * *").starts_with("expected 5 fields"));
        assert!(reason("* * * * * * *").starts_with("expected 5 fields"));
        assert!(reason("").starts_with("expected 5 fields"));
        assert!(reason("@reboot").starts_with("unknown macro `@reboot`"));
        assert!(reason("0 0 30 2 *").contains("never matches"));
        assert!(reason("0 0 31 4,6,9,11 *").contains("never matches"));
        assert!(reason("* * * * MON#6").contains("1 to 5"));
        assert!(reason("* * * * L").contains("ambiguous"));
        assert!(reason("* * L-31 * *").contains("L-n"));
        assert!(reason("* * 32W * *").contains("out of range"));
        #[cfg(not(feature = "tz"))]
        assert!(reason("CRON_TZ=Europe/Paris * * * * *").contains("`tz` Cargo feature"));
        #[cfg(feature = "tz")]
        assert!(reason("CRON_TZ=Mars/Olympus * * * * *").contains("unknown time zone"));
        let full = Cron::parse("0 25 * * *").unwrap_err().to_string();
        assert_eq!(
            full,
            "invalid cron expression `0 25 * * *`: hour `25` is out of range 0-23"
        );
    }

    #[test]
    fn exhaustive_against_brute_force() {
        // Compare the search with a minute-by-minute scan over two years,
        // for a mix of expressions.
        let exprs = [
            "*/7 3-5 * * *",
            "0 0 L * *",
            "30 12 1,15,L-1 * *",
            "0 6 13 * FRI",
            "0 6 */3 * MON-WED",
            "0 0 * * 5L",
            "0 8 * * TUE#3",
            "45 23 15W * *",
            "0 0 LW 2,8 *",
            "0 12 29 2 *",
            "0 0 * FEB SUN",
            "15 */6 * NOV-FEB *",
        ];
        let start = ts("2023-12-25 00:00:00");
        let end = ts("2025-03-10 00:00:00");
        for expr in exprs {
            let cron = Cron::parse(expr).unwrap();
            let mut expected = Vec::new();
            let mut t = start - start % 60 + 60;
            while t <= end {
                let (y, m, d) = civil_from_days(t.div_euclid(DAY));
                let sod = t.rem_euclid(DAY);
                if cron.seconds & 1 != 0
                    && cron.minutes & (1 << (sod / 60 % 60)) != 0
                    && cron.hours & (1 << (sod / 3600)) != 0
                    && cron.months & (1 << m) != 0
                    && cron.days.matches(y, m, d)
                {
                    expected.push(t);
                }
                t += 60;
            }
            let mut got = Vec::new();
            let mut at = start;
            while let Some(n) = cron.next_secs(at) {
                if n > end {
                    break;
                }
                got.push(n);
                at = n;
            }
            assert!(!expected.is_empty(), "{expr}");
            assert_eq!(got, expected, "{expr}");
        }
    }

    #[test]
    fn system_time_api() {
        let cron = Cron::parse("0 0 1 1 *").unwrap();
        let before_epoch = UNIX_EPOCH - Duration::from_millis(1500);
        assert_eq!(cron.next_after(before_epoch), Some(UNIX_EPOCH));
        let t = UNIX_EPOCH + Duration::from_millis(500);
        let every_second = Cron::parse("* * * * * *").unwrap();
        assert_eq!(
            every_second.next_after(t),
            Some(UNIX_EPOCH + Duration::from_secs(1))
        );
        assert_eq!(
            every_second.next_after(UNIX_EPOCH),
            Some(UNIX_EPOCH + Duration::from_secs(1))
        );
        let upcoming: Vec<_> = every_second.upcoming(UNIX_EPOCH).take(3).collect();
        assert_eq!(upcoming.len(), 3);
        assert_eq!(upcoming[2], UNIX_EPOCH + Duration::from_secs(3));
        // Far in the future there is no next run rather than a panic.
        let year_300000 = UNIX_EPOCH + Duration::from_secs(300_000 * 366 * 86_400);
        let _ = cron.next_after(year_300000);
    }

    #[cfg(feature = "tz")]
    mod zones {
        use super::*;

        fn zrun(expr: &str, from: &str, n: usize) -> Vec<String> {
            runs(expr, from, n)
        }

        #[test]
        fn named_zone_without_dst_change() {
            // Paris is UTC+1 in January.
            assert_eq!(
                zrun("CRON_TZ=Europe/Paris 0 9 * * *", "2024-01-10 00:00:00", 1),
                ["2024-01-10 08:00:00"]
            );
            // ... and UTC+2 in July.
            assert_eq!(
                zrun("CRON_TZ=Europe/Paris 0 9 * * *", "2024-07-10 00:00:00", 1),
                ["2024-07-10 07:00:00"]
            );
        }

        #[test]
        fn spring_forward_runs_skipped_times_at_the_jump() {
            // New York, 2024-03-10: 02:00 EST jumps to 03:00 EDT (07:00 UTC).
            assert_eq!(
                zrun(
                    "CRON_TZ=America/New_York 30 2 * * *",
                    "2024-03-09 12:00:00",
                    3
                ),
                [
                    "2024-03-10 07:00:00", // 02:30 doesn't exist: at the jump
                    "2024-03-11 06:30:00", // 02:30 EDT
                    "2024-03-12 06:30:00"
                ]
            );
            // Every 15 minutes: the skipped hour fires once, at the jump.
            assert_eq!(
                zrun(
                    "CRON_TZ=America/New_York */15 * * * *",
                    "2024-03-10 06:40:00",
                    4
                ),
                [
                    "2024-03-10 06:45:00", // 01:45 EST
                    "2024-03-10 07:00:00", // 03:00 EDT
                    "2024-03-10 07:15:00",
                    "2024-03-10 07:30:00"
                ]
            );
        }

        #[test]
        fn fall_back_runs_fixed_times_once_and_wildcard_hours_twice() {
            // New York, 2024-11-03: 02:00 EDT goes back to 01:00 EST (06:00 UTC).
            // 01:30 happens at 05:30 UTC (EDT) and 06:30 UTC (EST).
            assert_eq!(
                zrun(
                    "CRON_TZ=America/New_York 30 1 * * *",
                    "2024-11-02 12:00:00",
                    2
                ),
                ["2024-11-03 05:30:00", "2024-11-04 06:30:00"]
            );
            // Started during the second pass: still not repeated.
            assert_eq!(
                zrun(
                    "CRON_TZ=America/New_York 30 1 * * *",
                    "2024-11-03 06:10:00",
                    1
                ),
                ["2024-11-04 06:30:00"]
            );
            // Hour `*`: keeps running every 30 minutes through both passes.
            assert_eq!(
                zrun(
                    "CRON_TZ=America/New_York */30 * * * *",
                    "2024-11-03 05:00:00",
                    5
                ),
                [
                    "2024-11-03 05:30:00", // 01:30 EDT
                    "2024-11-03 06:00:00", // 01:00 EST
                    "2024-11-03 06:30:00", // 01:30 EST
                    "2024-11-03 07:00:00", // 02:00 EST
                    "2024-11-03 07:30:00"
                ]
            );
        }

        #[test]
        fn display_and_with_timezone() {
            let paris = Tz::parse("Europe/Paris").unwrap();
            let cron = Cron::parse("0 9 * * *")
                .unwrap()
                .with_timezone(paris.clone());
            assert_eq!(cron.to_string(), "CRON_TZ=Europe/Paris 0 9 * * *");
            assert_eq!(*cron.timezone(), paris);
            assert_eq!(Cron::parse(&cron.to_string()).unwrap(), cron);
        }
    }
}
