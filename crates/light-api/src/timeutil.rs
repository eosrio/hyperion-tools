//! Minimal UTC timestamp parsing — avoids pulling in chrono/time for one format.
//!
//! Hyperion/snapshot `@block_time` is rendered as `"YYYY-MM-DD HH:MM:SS"` (UTC, no timezone). We only
//! need head-block age = `now − block_time` in whole seconds for `/sync` and `/status`.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current unix time in whole seconds.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse `"YYYY-MM-DD HH:MM:SS"` (also tolerates a `T` separator and a trailing `Z`/fractional
/// seconds, as ISO timestamps sometimes appear) into unix seconds. `None` on malformation.
pub fn parse_utc(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (date, time) = s.split_once([' ', 'T'])?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Drop any trailing 'Z' / fractional seconds.
    let time = time.trim_end_matches('Z');
    let time = time.split('.').next()?;
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next().unwrap_or("0").parse().ok()?;
    if t.next().is_some()
        || !(0..24).contains(&hh)
        || !(0..60).contains(&mm)
        || !(0..=60).contains(&ss)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}

/// Head-block age in seconds (`now − block_time`), clamped at 0. `None` if `block_time` is empty or
/// unparseable (e.g. a snapshot-only DB that never filled `@block_time`).
pub fn age_secs(block_time: &str) -> Option<i64> {
    let t = parse_utc(block_time)?;
    Some((now_secs() - t).max(0))
}

/// Days since the unix epoch for a civil (proleptic Gregorian) date. Howard Hinnant's algorithm.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = ((m + 9) % 12) as i64; // Mar=0 … Feb=11
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch() {
        assert_eq!(parse_utc("1970-01-01 00:00:00"), Some(0));
    }

    #[test]
    fn known_timestamps() {
        // 2026-06-02 15:17:48 UTC — the value captured from the live /networks response.
        assert_eq!(parse_utc("2026-06-02 15:17:48"), Some(1_780_413_468));
        // 2000-01-01 00:00:00 UTC.
        assert_eq!(parse_utc("2000-01-01 00:00:00"), Some(946_684_800));
        // Leap day.
        assert_eq!(parse_utc("2020-02-29 12:00:00"), Some(1_582_977_600));
    }

    #[test]
    fn tolerates_iso_variants() {
        assert_eq!(
            parse_utc("2026-06-02T15:17:48.500Z"),
            parse_utc("2026-06-02 15:17:48")
        );
    }

    #[test]
    fn rejects_bad() {
        assert!(parse_utc("").is_none());
        assert!(parse_utc("not-a-date").is_none());
        assert!(parse_utc("2026-13-01 00:00:00").is_none());
    }
}
