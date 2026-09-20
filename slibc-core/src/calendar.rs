//! The proleptic-Gregorian civil calendar that `<time.h>` is defined over.
//!
//! The conversions are the era/day-of-era decomposition, the same shape the
//! kernel's RTC decoder uses so the two agree on every date. Everything is
//! `i64` with Euclidean division, which makes a pre-epoch date an ordinary
//! negative day count rather than a second code path.

const SECS_PER_DAY: i64 = 86_400;

/// Days in an era: 400 Gregorian years.
const DAYS_PER_ERA: i64 = 146_097;

/// Days from 0000-03-01 to 1970-01-01, the shift that puts the epoch on an
/// era boundary and so lets a leap day be the last day of the shifted year.
const EPOCH_SHIFT: i64 = 719_468;

/// A broken-down UTC time: the arithmetic fields of C's `struct tm`. The
/// three fields left out — `tm_isdst`, `tm_gmtoff`, `tm_zone` — are fixed by
/// SlopOS rather than computed, so they never enter the arithmetic.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Tm {
    pub sec: i32,
    pub min: i32,
    pub hour: i32,
    pub mday: i32,
    /// `0..=11`.
    pub mon: i32,
    /// Years since 1900.
    pub year: i32,
    /// `0` is Sunday.
    pub wday: i32,
    /// 0-based day of the year.
    pub yday: i32,
}

/// Days from 1970-01-01 to the proleptic-Gregorian `y-m-d`, with `m` in
/// `1..=12`.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let shifted = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * shifted + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * DAYS_PER_ERA + doe - EPOCH_SHIFT
}

/// The inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + EPOCH_SHIFT;
    let era = z.div_euclid(DAYS_PER_ERA);
    let doe = z - era * DAYS_PER_ERA;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m as u32, d)
}

/// `0` is Sunday. 1970-01-01, day zero, was a Thursday.
pub fn weekday_from_days(z: i64) -> u32 {
    (z + 4).rem_euclid(7) as u32
}

/// Breaks `secs` since the epoch into UTC fields. `None` when the resulting
/// `year - 1900` leaves `i32`.
pub fn utc_from_epoch(secs: i64) -> Option<Tm> {
    let days = secs.div_euclid(SECS_PER_DAY);
    let rem = secs.rem_euclid(SECS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let year = i32::try_from(y - 1900).ok()?;

    Some(Tm {
        sec: (rem % 60) as i32,
        min: (rem / 60 % 60) as i32,
        hour: (rem / 3600) as i32,
        mday: d as i32,
        mon: m as i32 - 1,
        year,
        wday: weekday_from_days(days) as i32,
        yday: (days - days_from_civil(y, 1, 1)) as i32,
    })
}

/// `mktime(3)`'s half: normalises `tm` in place and answers the epoch second
/// it names. Every field is a count, not a range-checked value, so `mday = 0`
/// is the last day of the previous month and `hour = 25` is tomorrow at one.
/// `None` on overflow, with `tm` left as the caller wrote it.
pub fn epoch_from_tm(tm: &mut Tm) -> Option<i64> {
    let mon = i64::from(tm.mon);
    let year = i64::from(tm.year)
        .checked_add(1900)?
        .checked_add(mon.div_euclid(12))?;
    let month = mon.rem_euclid(12) as u32 + 1;

    let days = days_from_civil(year, month, 1).checked_add(i64::from(tm.mday) - 1)?;
    let secs = days
        .checked_mul(SECS_PER_DAY)?
        .checked_add(i64::from(tm.hour).checked_mul(3600)?)?
        .checked_add(i64::from(tm.min).checked_mul(60)?)?
        .checked_add(i64::from(tm.sec))?;

    *tm = utc_from_epoch(secs)?;
    Some(secs)
}

/// The ISO-8601 week-based year and week number, `1..=53`. Week 1 is the week
/// containing 4 January, and a week runs Monday to Sunday.
pub fn iso_week(tm: &Tm) -> (i64, u32) {
    let year = i64::from(tm.year) + 1900;
    let iso_wday = i64::from(tm.wday.rem_euclid(7));
    let iso_wday = if iso_wday == 0 { 7 } else { iso_wday };
    let week = (i64::from(tm.yday) + 11 - iso_wday).div_euclid(7);

    let (year, week) = if week < 1 {
        (year - 1, iso_weeks_in_year(year - 1))
    } else if week > iso_weeks_in_year(year) {
        (year + 1, 1)
    } else {
        (year, week)
    };

    (year, week as u32)
}

/// 53 when the year begins on a Thursday, or on a Wednesday in a leap year;
/// 52 otherwise. `p` is the weekday of 31 December, `0` being Sunday.
fn iso_weeks_in_year(y: i64) -> i64 {
    let p = |y: i64| (y + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400)).rem_euclid(7);
    if p(y) == 4 || p(y - 1) == 3 { 53 } else { 52 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(year, month, day, days since 1970-01-01)`, both kinds of century
    /// boundary included.
    const CIVIL: [(i64, u32, u32, i64); 6] = [
        (1970, 1, 1, 0),
        (2020, 1, 1, 18262),
        (2024, 2, 29, 19782),
        (2000, 3, 1, 11017),
        (2100, 3, 1, 47541),
        (1900, 3, 1, -25508),
    ];

    #[test]
    fn civil_round_trips_known_days() {
        for (y, m, d, days) in CIVIL {
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
            assert_eq!(civil_from_days(days), (y, m, d), "day {days}");
        }
    }

    #[test]
    fn every_day_of_a_leap_year_round_trips() {
        let first = days_from_civil(2000, 1, 1);
        for z in first..first + 366 {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
            assert_eq!(y, 2000);
        }
        assert_eq!(civil_from_days(first + 59), (2000, 2, 29));
        assert_eq!(civil_from_days(first + 365), (2000, 12, 31));
    }

    #[test]
    fn weekdays_anchor_on_thursday() {
        assert_eq!(weekday_from_days(0), 4);
        assert_eq!(weekday_from_days(-1), 3);
        assert_eq!(weekday_from_days(11017), 3);
        assert_eq!(weekday_from_days(-25567), 1);
    }

    #[test]
    fn epoch_breaks_down_to_known_stamps() {
        let t = utc_from_epoch(0).unwrap();
        assert_eq!((t.year, t.mon, t.mday), (70, 0, 1));
        assert_eq!((t.hour, t.min, t.sec), (0, 0, 0));
        assert_eq!((t.wday, t.yday), (4, 0));

        let t = utc_from_epoch(-1).unwrap();
        assert_eq!((t.year, t.mon, t.mday), (69, 11, 31));
        assert_eq!((t.hour, t.min, t.sec), (23, 59, 59));
        assert_eq!((t.wday, t.yday), (3, 364));

        let t = utc_from_epoch(1_600_000_000).unwrap();
        assert_eq!((t.year, t.mon, t.mday), (120, 8, 13));
        assert_eq!((t.hour, t.min, t.sec), (12, 26, 40));
        assert_eq!(t.wday, 0);

        let t = utc_from_epoch(2_147_483_647).unwrap();
        assert_eq!((t.year, t.mon, t.mday), (138, 0, 19));
        assert_eq!((t.hour, t.min, t.sec), (3, 14, 7));

        let t = utc_from_epoch(253_402_300_799).unwrap();
        assert_eq!((t.year, t.mon, t.mday), (8099, 11, 31));
        assert_eq!((t.hour, t.min, t.sec), (23, 59, 59));
    }

    #[test]
    fn a_year_out_of_int_range_is_refused() {
        assert!(utc_from_epoch(i64::MAX).is_none());
        assert!(utc_from_epoch(i64::MIN).is_none());
    }

    #[test]
    fn breakdown_round_trips_across_the_range() {
        let stamps = [
            -2_208_988_800, // 1900-01-01
            -86_401,
            -1,
            0,
            951_782_400,   // 2000-02-29
            1_600_000_000, // 2020-09-13
            2_147_483_647, // the 32-bit cliff
            4_102_444_800, // 2100-01-01
            253_402_300_799,
        ];
        for secs in stamps {
            let mut tm = utc_from_epoch(secs).unwrap();
            let before = tm;
            assert_eq!(epoch_from_tm(&mut tm), Some(secs), "{secs}");
            assert_eq!(tm, before, "{secs} was renormalised");
        }
    }

    #[test]
    fn nineteen_hundred_is_not_a_leap_year() {
        let tm = utc_from_epoch(-2_208_988_800).unwrap();
        assert_eq!((tm.year, tm.mon, tm.mday, tm.wday), (0, 0, 1, 1));
        assert_eq!(
            days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 1),
            28
        );
    }

    fn at(year: i32, mon: i32, mday: i32, hour: i32, min: i32, sec: i32) -> Tm {
        Tm {
            sec,
            min,
            hour,
            mday,
            mon,
            year,
            wday: 0,
            yday: 0,
        }
    }

    /// Every field of `tm` is a count rather than a range-checked value.
    #[test]
    fn out_of_range_fields_normalise_the_way_c_says() {
        let mut tm = at(100, 0, 0, 0, 0, 0);
        let secs = epoch_from_tm(&mut tm).unwrap();
        assert_eq!((tm.year, tm.mon, tm.mday), (99, 11, 31));
        assert_eq!(secs, days_from_civil(1999, 12, 31) * SECS_PER_DAY);

        let mut tm = at(120, 12, 1, 0, 0, 0);
        epoch_from_tm(&mut tm).unwrap();
        assert_eq!((tm.year, tm.mon, tm.mday), (121, 0, 1));

        let mut tm = at(120, -1, 1, 0, 0, 0);
        epoch_from_tm(&mut tm).unwrap();
        assert_eq!((tm.year, tm.mon, tm.mday), (119, 11, 1));

        let mut tm = at(70, 0, 1, 25, 0, 0);
        assert_eq!(epoch_from_tm(&mut tm), Some(25 * 3600));
        assert_eq!((tm.mday, tm.hour, tm.wday), (2, 1, 5));

        let mut tm = at(70, 0, 1, 0, 0, 90);
        assert_eq!(epoch_from_tm(&mut tm), Some(90));
        assert_eq!((tm.min, tm.sec), (1, 30));

        let mut tm = at(70, 0, 1, 0, -1, -1);
        assert_eq!(epoch_from_tm(&mut tm), Some(-61));
        assert_eq!((tm.year, tm.mon, tm.mday), (69, 11, 31));
        assert_eq!((tm.hour, tm.min, tm.sec), (23, 58, 59));

        let mut tm = at(70, 0, 366, 0, 0, 0);
        epoch_from_tm(&mut tm).unwrap();
        assert_eq!((tm.year, tm.mon, tm.mday, tm.yday), (71, 0, 1, 0));
    }

    #[test]
    fn overflowing_fields_are_refused() {
        let mut tm = at(i32::MAX, i32::MAX, i32::MAX, i32::MAX, i32::MAX, i32::MAX);
        let before = tm;
        assert!(epoch_from_tm(&mut tm).is_none());
        assert_eq!(tm, before);

        let mut tm = at(i32::MIN, i32::MIN, i32::MIN, i32::MIN, i32::MIN, i32::MIN);
        let before = tm;
        assert!(epoch_from_tm(&mut tm).is_none());
        assert_eq!(tm, before);
    }

    fn iso_of(y: i64, m: u32, d: u32) -> (i64, u32) {
        let tm = utc_from_epoch(days_from_civil(y, m, d) * SECS_PER_DAY).unwrap();
        iso_week(&tm)
    }

    #[test]
    fn iso_weeks_spill_across_the_year_boundary() {
        assert_eq!(iso_of(2019, 12, 30), (2020, 1));
        assert_eq!(iso_of(2020, 1, 1), (2020, 1));
        assert_eq!(iso_of(2020, 12, 31), (2020, 53));
        assert_eq!(iso_of(2021, 1, 1), (2020, 53));
        assert_eq!(iso_of(2021, 1, 4), (2021, 1));
        assert_eq!(iso_of(2016, 1, 1), (2015, 53));
        assert_eq!(iso_of(1970, 1, 1), (1970, 1));
        assert_eq!(iso_of(2000, 1, 1), (1999, 52));
        assert_eq!(iso_of(2100, 1, 1), (2099, 53));
    }

    #[test]
    fn iso_week_one_always_contains_the_fourth_of_january() {
        for y in 1970..2100 {
            assert_eq!(iso_of(y, 1, 4), (y, 1), "{y}");
            assert_eq!(iso_of(y, 12, 28).0, y, "{y}");
        }
    }
}
