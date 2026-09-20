//! Timestamp fields for the utilities; the calendar arithmetic itself is
//! [`slopos_slibc_core::calendar`]. SlopOS keeps no timezone database, so a
//! stamp is always UTC.

use slopos_slibc_core::calendar;

/// A Unix timestamp split into UTC calendar fields.
pub struct Utc {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 = Sunday.
    pub weekday: u32,
    /// 1-366.
    pub yday: u32,
}

/// Floor division throughout, so a pre-1970 timestamp lands on the day that
/// contains it rather than the one after it.
pub fn utc_from_epoch(secs: i64) -> Utc {
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (year, month, day) = calendar::civil_from_days(days);
    Utc {
        year,
        month,
        day,
        hour: (rest / 3_600) as u32,
        minute: (rest / 60 % 60) as u32,
        second: (rest % 60) as u32,
        weekday: calendar::weekday_from_days(days),
        yday: (days - calendar::days_from_civil(year, 1, 1) + 1) as u32,
    }
}

pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
