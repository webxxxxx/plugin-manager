//! IMF-fixdate formatting and parsing (`Sun, 06 Nov 1994 08:49:37 GMT`).
//!
//! `model` re-exports the two public functions, so callers keep writing
//! `model::http_date` and the date codec has one home.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Format an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`).
pub fn http_date(time: SystemTime) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday; WEEKDAYS starts with Thursday.
    let weekday = days.rem_euclid(7) as usize;
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[weekday],
        day,
        MONTHS[(month - 1) as usize],
        year,
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Parse the IMF-fixdate form emitted by `http_date`. Other obsolete HTTP date
/// forms are ignored (callers then fall back to treating the condition as
/// absent), which is allowed by RFC 9110.
pub fn parse_http_date(text: &str) -> Option<SystemTime> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = text.split_whitespace();
    let _weekday = parts.next()?;
    let day: u32 = parts.next()?.trim_end_matches(',').parse().ok()?;
    let month_name = parts.next()?;
    let month = MONTHS
        .iter()
        .position(|name| name.eq_ignore_ascii_case(month_name))? as u32
        + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    if !parts.next()?.eq_ignore_ascii_case("GMT") || parts.next().is_some() {
        return None;
    }
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add((hour * 3600 + minute * 60 + second) as i64)?;
    if seconds < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to Y/M/D.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_dates_round_trip() {
        for text in [
            "Thu, 01 Jan 1970 00:00:00 GMT",
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sun, 15 Nov 2026 12:00:00 GMT",
        ] {
            let parsed = parse_http_date(text).expect("date");
            assert_eq!(http_date(parsed), text);
        }
        assert!(parse_http_date("not a date").is_none());
        assert!(parse_http_date("Sun, 06 Nov 1994 08:49:37 PST").is_none());
    }
}
