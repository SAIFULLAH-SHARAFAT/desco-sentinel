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
