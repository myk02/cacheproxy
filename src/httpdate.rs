//! HTTP dates (RFC 9110 section 5.6.7).
//!
//! Parses all three formats a recipient must accept and formats the
//! preferred one (IMF-fixdate). Times are seconds since the Unix epoch.

use std::time::{SystemTime, UNIX_EPOCH};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Current wall-clock time as Unix seconds.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse an HTTP date. Accepts:
/// * IMF-fixdate: `Sun, 06 Nov 1994 08:49:37 GMT`
/// * RFC 850:     `Sunday, 06-Nov-94 08:49:37 GMT`
/// * asctime:     `Sun Nov  6 08:49:37 1994`
pub fn parse(s: &str) -> Option<u64> {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    match tokens.as_slice() {
        // IMF-fixdate
        [wd, day, mon, year, time, "GMT"] if wd.ends_with(',') => {
            build(year.parse().ok()?, month(mon)?, day.parse().ok()?, time)
        }
        // RFC 850
        [wd, date, time, "GMT"] if wd.ends_with(',') => {
            let mut p = date.split('-');
            let day: i64 = p.next()?.parse().ok()?;
            let mon = month(p.next()?)?;
            let mut year: i64 = p.next()?.parse().ok()?;
            if p.next().is_some() {
                return None;
            }
            if year < 100 {
                year += if year < 70 { 2000 } else { 1900 };
            }
            build(year, mon, day, time)
        }
        // asctime
        [_wd, mon, day, time, year] => {
            build(year.parse().ok()?, month(mon)?, day.parse().ok()?, time)
        }
        _ => None,
    }
}

fn month(name: &str) -> Option<i64> {
    MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(name))
        .map(|i| i as i64 + 1)
}

fn build(year: i64, month: i64, day: i64, time: &str) -> Option<u64> {
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let m: i64 = t.next()?.parse().ok()?;
    let s: i64 = t.next()?.parse().ok()?;
    if t.next().is_some()
        || !(0..24).contains(&h)
        || !(0..60).contains(&m)
        || !(0..=60).contains(&s)
    {
        return None;
    }
    if !(1..=31).contains(&day) || year < 1970 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + h * 3600 + m * 60 + s;
    u64::try_from(secs).ok()
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format Unix seconds as an IMF-fixdate string.
pub fn format(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let rem = unix % 86_400;
    let (y, m, d) = civil_from_days(days);
    let wd = (days + 4).rem_euclid(7) as usize; // 1970-01-01 was a Thursday
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[wd],
        d,
        MONTHS[(m - 1) as usize],
        y,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const REF: u64 = 784_111_777; // Sun, 06 Nov 1994 08:49:37 GMT

    #[test]
    fn imf_fixdate() {
        assert_eq!(parse("Sun, 06 Nov 1994 08:49:37 GMT"), Some(REF));
    }

    #[test]
    fn rfc850() {
        assert_eq!(parse("Sunday, 06-Nov-94 08:49:37 GMT"), Some(REF));
    }

    #[test]
    fn asctime() {
        assert_eq!(parse("Sun Nov  6 08:49:37 1994"), Some(REF));
    }

    #[test]
    fn epoch_start() {
        assert_eq!(parse("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
    }

    #[test]
    fn leap_day() {
        // 2024-02-29 00:00:00 UTC
        assert_eq!(parse("Thu, 29 Feb 2024 00:00:00 GMT"), Some(1_709_164_800));
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("tomorrow"), None);
        assert_eq!(parse("Sun, 06 Foo 1994 08:49:37 GMT"), None);
        assert_eq!(parse("Sun, 06 Nov 1994 25:49:37 GMT"), None);
        assert_eq!(parse("Sun, 32 Nov 1994 08:49:37 GMT"), None);
        assert_eq!(parse("Sun, 06 Nov 1994 08:49:37 PST"), None);
    }

    #[test]
    fn format_round_trip() {
        assert_eq!(format(REF), "Sun, 06 Nov 1994 08:49:37 GMT");
        for t in [0, 86_399, 1_709_164_800, 1_800_000_000, 4_102_444_800] {
            assert_eq!(parse(&format(t)), Some(t));
        }
    }

    #[test]
    fn now_is_after_2020() {
        assert!(now_unix() > 1_577_836_800);
    }
}
