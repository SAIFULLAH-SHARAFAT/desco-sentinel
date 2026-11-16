//! Asia/Dhaka wall-clock helpers. Bangladesh has no DST, so a fixed UTC+6 offset is exact.

use time::macros::{format_description, offset};
use time::{Duration, OffsetDateTime};

#[must_use]
pub fn dhaka_now() -> OffsetDateTime {
    OffsetDateTime::now_utc().to_offset(offset!(+6))
}

/// `2026-10-18 14:05`
#[must_use]
pub fn format_minute(t: OffsetDateTime) -> String {
    t.format(format_description!("[year]-[month]-[day] [hour]:[minute]"))
        .unwrap_or_default()
}

/// `2026-10-18`
#[must_use]
pub fn format_day(t: OffsetDateTime) -> String {
    t.format(format_description!("[year]-[month]-[day]"))
        .unwrap_or_default()
}

/// `2026-10`
#[must_use]
pub fn format_month(t: OffsetDateTime) -> String {
    t.format(format_description!("[year]-[month]"))
        .unwrap_or_default()
}

#[must_use]
pub fn days_ago(days: i64) -> OffsetDateTime {
    dhaka_now() - Duration::days(days)
}

/// Converts a portal timestamp such as `2026-11-16T00:09:13.056Z` to Dhaka time
/// (`2026-11-16 06:09`). Text that is not RFC 3339 is returned unchanged.
#[must_use]
pub fn rfc3339_to_dhaka(text: &str) -> String {
    OffsetDateTime::parse(text.trim(), &time::format_description::well_known::Rfc3339).map_or_else(
        |_| text.to_string(),
        |t| format_minute(t.to_offset(offset!(+6))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_utc_times_become_dhaka_times() {
        assert_eq!(
            rfc3339_to_dhaka("2026-11-16T00:09:13.056Z"),
            "2026-11-16 06:09"
        );
        assert_eq!(
            rfc3339_to_dhaka("2026-11-15T00:00:00+06:00"),
            "2026-11-15 00:00"
        );
        assert_eq!(rfc3339_to_dhaka("not a time"), "not a time");
    }
}
