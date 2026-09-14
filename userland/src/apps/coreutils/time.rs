//! Civil-date conversion, shared by every utility that renders a timestamp.
//! SlopOS keeps no timezone database, so a stamp is always UTC.

/// Howard Hinnant's `civil_from_days`: era arithmetic, exact for every
/// representable day and needing no leap table.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

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
    let (year, month, day) = civil_from_days(days);
    Utc {
        year,
        month,
        day,
        hour: (rest / 3_600) as u32,
        minute: (rest / 60 % 60) as u32,
        second: (rest % 60) as u32,
        // 1970-01-01 was a Thursday.
        weekday: (days + 4).rem_euclid(7) as u32,
        yday: day_of_year(year, month, day),
    }
}

fn day_of_year(year: i64, month: u32, day: u32) -> u32 {
    const CUMULATIVE: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    CUMULATIVE[month as usize - 1] + day + u32::from(leap && month > 2)
}

pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
