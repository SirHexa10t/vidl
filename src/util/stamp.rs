//! The filename timestamp — `YYYY-mm-DD_HHMM` in UTC.
//!
//! Copied from the project this was extracted from, and for the same reason it existed there: a
//! date crate would be a dependency for one format string. The calendar arithmetic is Howard
//! Hinnant's days-to-civil algorithm.

use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn datehour_stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format_utc(secs)
}

/// `secs` since the Unix epoch → `YYYY-mm-DD_HHMM` in UTC. Split from [`datehour_stamp`] so it's
/// testable against fixed instants (the wall clock isn't). The calendar date comes from Howard
/// Hinnant's days-to-civil algorithm, so no date crate is pulled in.
fn format_utc(secs: u64) -> String {
    let (days, sec_of_day) = ((secs / 86_400) as i64, secs % 86_400);
    let (hour, minute) = (sec_of_day / 3_600, (sec_of_day % 3_600) / 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}_{hour:02}{minute:02}")
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_instant_formats_to_its_utc_calendar_date() {
        // 2021-06-02T07:56:21Z — a fixed instant, so this tests the arithmetic and not the clock.
        assert_eq!(format_utc(1_622_620_581), "2021-06-02_0756");
        assert_eq!(format_utc(0), "1970-01-01_0000", "the epoch itself");
        // A leap day must not slide by one.
        assert_eq!(format_utc(1_709_208_000), "2024-02-29_1200", "a leap day must not slide by one");
    }
}
