//! Byte counts as a person would say them.
//!
//! Binary units, to match what yt-dlp's own progress lines print — the numbers this crate reports
//! are usually read right next to one of those, and two conventions on one screen invite the
//! wrong comparison.

/// A byte count in the largest unit that leaves it above one.
///
/// Bytes stay whole: `1023 B` rather than `1023.0 B`, because a decimal on an exact count reads as
/// a rounding of something else.
pub(crate) fn roughly(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{size:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_byte_count_is_stated_the_way_the_downloader_states_it() {
        assert_eq!(roughly(749_233_884), "714.5 MiB");
        assert_eq!(roughly(0), "0 B");
        assert_eq!(roughly(1023), "1023 B", "bytes stay whole — `1023.0 B` reads as a rounding");
        assert_eq!(roughly(1024), "1.0 KiB");
        // It must not run off the end of the unit table.
        assert!(roughly(u64::MAX).ends_with(" GiB"), "{}", roughly(u64::MAX));
    }
}
