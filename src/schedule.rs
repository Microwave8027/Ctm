//! Job schedules: cron expressions (with time zones), fixed intervals,
//! one-shot times, and manual-only jobs.

use std::{fmt, str::FromStr, time::Duration};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, Default)]
#[sqlx(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ScheduleKind {
    /// Cron expression: 5 fields, 6 with seconds, or `@hourly`/`@daily`/...
    Cron,
    /// Fixed interval such as `15m`, `1h30m`, `2d`.
    Interval,
    /// A single point in time (RFC 3339, or `YYYY-MM-DD HH:MM` in the job's zone).
    Once,
    /// Only runs when triggered by hand, by the API, or by another job.
    #[default]
    Manual,
}

impl ScheduleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScheduleKind::Cron => "cron",
            ScheduleKind::Interval => "interval",
            ScheduleKind::Once => "once",
            ScheduleKind::Manual => "manual",
        }
    }
}

impl fmt::Display for ScheduleKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ScheduleKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "cron" => ScheduleKind::Cron,
            "interval" => ScheduleKind::Interval,
            "once" => ScheduleKind::Once,
            "manual" | "" => ScheduleKind::Manual,
            other => bail!("unknown schedule kind {other:?} (cron, interval, once, manual)"),
        })
    }
}

/// A parsed, validated schedule.
#[derive(Debug, Clone)]
pub enum Schedule {
    Cron(Box<croner::Cron>, Tz),
    Interval(Duration),
    Once(DateTime<Utc>),
    Manual,
}

impl Schedule {
    pub fn parse(kind: ScheduleKind, expr: Option<&str>, timezone: &str) -> Result<Self> {
        let tz: Tz = timezone.trim().parse().map_err(|_| {
            anyhow::anyhow!("unknown time zone {timezone:?} (use an IANA name like Europe/Berlin)")
        })?;
        let expr = expr.map(str::trim).filter(|s| !s.is_empty());
        let need =
            |what: &str| expr.with_context(|| format!("a {what} schedule needs an expression"));
        Ok(match kind {
            ScheduleKind::Manual => Schedule::Manual,
            ScheduleKind::Cron => {
                let e = need("cron")?;
                let cron = croner::Cron::from_str(e)
                    .with_context(|| format!("invalid cron expression {e:?}"))?;
                Schedule::Cron(Box::new(cron), tz)
            }
            ScheduleKind::Interval => {
                let d = parse_interval(need("interval")?)?;
                if d < Duration::from_secs(10) {
                    bail!("intervals shorter than 10s are not allowed");
                }
                Schedule::Interval(d)
            }
            ScheduleKind::Once => Schedule::Once(parse_datetime(need("once")?, tz)?),
        })
    }

    /// The next fire time strictly after `after`, or `None` if the schedule
    /// will never fire again.
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        // Fire on whole seconds.
        let after = after.with_nanosecond(0).unwrap_or(after);
        Ok(match self {
            Schedule::Manual => None,
            Schedule::Once(at) => (*at > after).then_some(*at),
            Schedule::Interval(d) => Some(after + chrono::Duration::from_std(*d)?),
            Schedule::Cron(cron, tz) => {
                let local = after.with_timezone(tz);
                Some(
                    cron.find_next_occurrence(&local, false)
                        .context("cron expression never fires")?
                        .with_timezone(&Utc),
                )
            }
        })
    }

    /// The next `n` fire times after `from` (for previews).
    pub fn upcoming(&self, from: DateTime<Utc>, n: usize) -> Vec<DateTime<Utc>> {
        let mut out = Vec::with_capacity(n);
        let mut t = from;
        while out.len() < n {
            match self.next_after(t) {
                Ok(Some(next)) => {
                    out.push(next);
                    t = next;
                }
                _ => break,
            }
        }
        out
    }

    pub fn describe(&self) -> String {
        match self {
            Schedule::Manual => "manual only".into(),
            Schedule::Once(at) => format!("once at {}", at.format("%Y-%m-%d %H:%M:%S UTC")),
            Schedule::Interval(d) => format!("every {}", format_interval(*d)),
            Schedule::Cron(c, tz) => format!("{} ({tz})", c.describe()),
        }
    }
}

/// Parses `90s`, `15m`, `1h30m`, `2d`, `1w` (units may be combined).
pub fn parse_interval(s: &str) -> Result<Duration> {
    let s = s.trim().to_ascii_lowercase();
    let s = s.strip_prefix("every ").unwrap_or(&s).replace(' ', "");
    if s.is_empty() {
        bail!("empty interval");
    }
    let mut total: u64 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num
            .parse()
            .with_context(|| format!("invalid interval {s:?}: expected a number before {c:?}"))?;
        num.clear();
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            _ => bail!("invalid interval unit {c:?} in {s:?} (use s, m, h, d, w)"),
        };
        total = total.saturating_add(n.saturating_mul(unit));
    }
    if !num.is_empty() {
        bail!("invalid interval {s:?}: missing unit after {num}");
    }
    Ok(Duration::from_secs(total))
}

pub fn format_interval(d: Duration) -> String {
    let mut s = d.as_secs();
    let mut out = String::new();
    for (unit, secs) in [("d", 86_400), ("h", 3600), ("m", 60), ("s", 1)] {
        if s >= secs {
            out.push_str(&format!("{}{unit}", s / secs));
            s %= secs;
        }
    }
    if out.is_empty() { "0s".into() } else { out }
}

/// Parses RFC 3339, or a naive `YYYY-MM-DD HH:MM[:SS]` / `YYYY-MM-DDTHH:MM`
/// interpreted in `tz`.
pub fn parse_datetime(s: &str, tz: Tz) -> Result<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return tz
                .from_local_datetime(&naive)
                .earliest()
                .map(|d| d.with_timezone(&Utc))
                .with_context(|| format!("{s:?} does not exist in {tz}"));
        }
    }
    bail!("invalid time {s:?} (use RFC 3339 or YYYY-MM-DD HH:MM)")
}

/// A cheap pseudo-random offset in `0..=max_secs` for schedule jitter.
pub fn jitter(max_secs: i64) -> chrono::Duration {
    if max_secs <= 0 {
        return chrono::Duration::zero();
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i64)
        .unwrap_or(0);
    chrono::Duration::seconds(nanos % (max_secs + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals() {
        assert_eq!(parse_interval("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_interval("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(
            parse_interval("every 2d").unwrap(),
            Duration::from_secs(172_800)
        );
        assert!(parse_interval("15").is_err());
        assert!(parse_interval("5x").is_err());
        assert_eq!(format_interval(Duration::from_secs(5400)), "1h30m");
    }

    #[test]
    fn cron_respects_time_zone() {
        let s = Schedule::parse(ScheduleKind::Cron, Some("0 9 * * *"), "America/New_York").unwrap();
        let from = Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap();
        // 09:00 EST == 14:00 UTC in January.
        assert_eq!(
            s.next_after(from).unwrap().unwrap(),
            Utc.with_ymd_and_hms(2026, 1, 15, 14, 0, 0).unwrap()
        );
        assert_eq!(s.upcoming(from, 3).len(), 3);
        assert!(Schedule::parse(ScheduleKind::Cron, Some("@daily"), "UTC").is_ok());
        assert!(Schedule::parse(ScheduleKind::Cron, Some("nope"), "UTC").is_err());
        assert!(Schedule::parse(ScheduleKind::Cron, Some("* * * * *"), "Mars/Base").is_err());
    }

    #[test]
    fn once_fires_once() {
        let s = Schedule::parse(
            ScheduleKind::Once,
            Some("2026-03-01 08:30"),
            "Europe/Berlin",
        )
        .unwrap();
        let at = Utc.with_ymd_and_hms(2026, 3, 1, 7, 30, 0).unwrap();
        assert_eq!(
            s.next_after(at - chrono::Duration::seconds(1)).unwrap(),
            Some(at)
        );
        assert_eq!(s.next_after(at).unwrap(), None);
        assert!(Schedule::parse(ScheduleKind::Interval, Some("5s"), "UTC").is_err());
        assert!(
            Schedule::parse(ScheduleKind::Manual, None, "UTC")
                .unwrap()
                .next_after(at)
                .unwrap()
                .is_none()
        );
    }
}
