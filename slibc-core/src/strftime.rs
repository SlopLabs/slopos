//! `strftime(3)` and `asctime(3)` over the C locale.
//!
//! The tables below are both halves of one fact: they are what a conversion
//! renders and what `nl_langinfo` answers, so [`crate::strftime`] is the only
//! place the C locale's day and month names are spelled.

use crate::calendar::{Tm, epoch_from_tm, iso_week};

pub const ABDAY: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

pub const DAY: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

pub const ABMON: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub const MON: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

pub const AM_PM: [&str; 2] = ["AM", "PM"];

pub const D_T_FMT: &str = "%a %b %e %T %Y";
pub const D_FMT: &str = "%m/%d/%y";
pub const T_FMT: &str = "%H:%M:%S";
pub const T_FMT_AMPM: &str = "%I:%M:%S %p";

/// `%Z`'s answer, asked for only when a format actually reaches `%Z`.
///
/// `tm_zone` is a BSD extension, not one of C17 7.27.1's nine members, so a
/// conforming caller may leave it indeterminate — reading it eagerly turns
/// `strftime(buf, n, "%Y", &t)` into a walk off a garbage pointer.
pub trait Zone {
    fn name(&self) -> &[u8];
}

impl Zone for [u8] {
    fn name(&self) -> &[u8] {
        self
    }
}

/// Renders `fmt` and a terminating NUL into `out`; the length excludes the
/// NUL. `None` when the result does not fit, in which case `out` holds an
/// unspecified prefix — exactly what C says about a `strftime` that returns 0.
pub fn format<Z: Zone + ?Sized>(
    out: &mut [u8],
    fmt: &[u8],
    tm: &Tm,
    gmtoff: i64,
    zone: &Z,
) -> Option<usize> {
    let mut sink = Sink::new(out)?;
    if !render(&mut sink, fmt, tm, gmtoff, zone) {
        return None;
    }
    Some(sink.finish())
}

/// `asctime(3)`'s fixed form: 25 characters and a NUL for a four-digit year.
/// `false`, having written nothing usable, when the year needs more room than
/// that — the caller reports `EOVERFLOW` rather than truncating a date.
pub fn asctime(tm: &Tm, out: &mut [u8; 26]) -> bool {
    let Some(mut sink) = Sink::new(out) else {
        return false;
    };
    let ok = sink.str(ABDAY[weekday(tm)])
        && sink.byte(b' ')
        && sink.str(ABMON[month(tm)])
        && sink.num(i64::from(tm.mday), 3, b' ')
        && sink.byte(b' ')
        && sink.num(i64::from(tm.hour), 2, b'0')
        && sink.byte(b':')
        && sink.num(i64::from(tm.min), 2, b'0')
        && sink.byte(b':')
        && sink.num(i64::from(tm.sec), 2, b'0')
        && sink.byte(b' ')
        && sink.num(i64::from(tm.year) + 1900, 0, b'0')
        && sink.byte(b'\n');
    if !ok {
        return false;
    }
    sink.finish();
    true
}

/// A caller-owned buffer with one byte held back for the NUL.
struct Sink<'a> {
    buf: &'a mut [u8],
    cap: usize,
    len: usize,
}

impl<'a> Sink<'a> {
    fn new(buf: &'a mut [u8]) -> Option<Self> {
        let cap = buf.len().checked_sub(1)?;
        Some(Self { buf, cap, len: 0 })
    }

    fn byte(&mut self, b: u8) -> bool {
        if self.len >= self.cap {
            return false;
        }
        self.buf[self.len] = b;
        self.len += 1;
        true
    }

    fn str(&mut self, s: &str) -> bool {
        self.bytes(s.as_bytes())
    }

    fn bytes(&mut self, s: &[u8]) -> bool {
        for &b in s {
            if !self.byte(b) {
                return false;
            }
        }
        true
    }

    /// A minus sign leads zero padding and trails space padding, as in
    /// `printf`.
    fn num(&mut self, value: i64, width: usize, pad: u8) -> bool {
        let mut digits = [0u8; 20];
        let mut rest = value.unsigned_abs();
        let mut n = 0;
        loop {
            digits[n] = b'0' + (rest % 10) as u8;
            rest /= 10;
            n += 1;
            if rest == 0 {
                break;
            }
        }

        let negative = value < 0;
        let printed = n + usize::from(negative);
        if negative && pad == b'0' && !self.byte(b'-') {
            return false;
        }
        for _ in printed..width {
            if !self.byte(pad) {
                return false;
            }
        }
        if negative && pad != b'0' && !self.byte(b'-') {
            return false;
        }
        while n > 0 {
            n -= 1;
            if !self.byte(digits[n]) {
                return false;
            }
        }
        true
    }

    fn finish(self) -> usize {
        self.buf[self.len] = 0;
        self.len
    }
}

fn weekday(tm: &Tm) -> usize {
    tm.wday.rem_euclid(7) as usize
}

fn month(tm: &Tm) -> usize {
    tm.mon.rem_euclid(12) as usize
}

fn expansion(conv: u8) -> Option<&'static str> {
    match conv {
        b'c' => Some(D_T_FMT),
        b'D' => Some(D_FMT),
        b'F' => Some("%Y-%m-%d"),
        b'r' => Some(T_FMT_AMPM),
        b'R' => Some("%H:%M"),
        b'T' => Some(T_FMT),
        b'x' => Some(D_FMT),
        b'X' => Some(T_FMT),
        _ => None,
    }
}

fn render<Z: Zone + ?Sized>(
    sink: &mut Sink<'_>,
    fmt: &[u8],
    tm: &Tm,
    gmtoff: i64,
    zone: &Z,
) -> bool {
    let mut i = 0;
    while i < fmt.len() {
        let b = fmt[i];
        i += 1;
        if b != b'%' {
            if !sink.byte(b) {
                return false;
            }
            continue;
        }
        if i == fmt.len() {
            return sink.byte(b'%');
        }

        let mut conv = fmt[i];
        i += 1;
        let mut modifier = None;
        if matches!(conv, b'E' | b'O') && i < fmt.len() {
            modifier = Some(conv);
            conv = fmt[i];
            i += 1;
        }

        if !convert(sink, conv, modifier, tm, gmtoff, zone) {
            return false;
        }
    }
    true
}

/// The `E` and `O` modifiers are dropped rather than honoured: the C locale
/// has neither an era nor alternative digits, so the unmodified conversion
/// *is* the modified one's answer.
fn convert<Z: Zone + ?Sized>(
    sink: &mut Sink<'_>,
    conv: u8,
    modifier: Option<u8>,
    tm: &Tm,
    gmtoff: i64,
    zone: &Z,
) -> bool {
    if let Some(sub) = expansion(conv) {
        return render(sink, sub.as_bytes(), tm, gmtoff, zone);
    }

    let year = i64::from(tm.year) + 1900;
    let yday = i64::from(tm.yday);
    let wday = i64::from(tm.wday.rem_euclid(7));

    match conv {
        b'a' => sink.str(ABDAY[weekday(tm)]),
        b'A' => sink.str(DAY[weekday(tm)]),
        b'b' | b'h' => sink.str(ABMON[month(tm)]),
        b'B' => sink.str(MON[month(tm)]),
        b'C' => sink.num(year.div_euclid(100), 2, b'0'),
        b'd' => sink.num(i64::from(tm.mday), 2, b'0'),
        b'e' => sink.num(i64::from(tm.mday), 2, b' '),
        b'g' => sink.num(iso_week(tm).0.rem_euclid(100), 2, b'0'),
        b'G' => sink.num(iso_week(tm).0, 4, b'0'),
        b'H' => sink.num(i64::from(tm.hour), 2, b'0'),
        b'I' => {
            let hour = tm.hour.rem_euclid(12);
            sink.num(i64::from(if hour == 0 { 12 } else { hour }), 2, b'0')
        }
        b'j' => sink.num(yday + 1, 3, b'0'),
        b'm' => sink.num(i64::from(tm.mon) + 1, 2, b'0'),
        b'M' => sink.num(i64::from(tm.min), 2, b'0'),
        b'n' => sink.byte(b'\n'),
        b'p' => sink.str(AM_PM[usize::from(tm.hour.rem_euclid(24) >= 12)]),
        b's' => {
            let mut copy = *tm;
            let secs = epoch_from_tm(&mut copy).map_or(-1, |s| s.saturating_sub(gmtoff));
            sink.num(secs, 0, b'0')
        }
        b'S' => sink.num(i64::from(tm.sec), 2, b'0'),
        b't' => sink.byte(b'\t'),
        b'u' => sink.num(if wday == 0 { 7 } else { wday }, 0, b'0'),
        b'U' => sink.num((yday + 7 - wday).div_euclid(7), 2, b'0'),
        b'V' => sink.num(i64::from(iso_week(tm).1), 2, b'0'),
        b'w' => sink.num(wday, 0, b'0'),
        b'W' => sink.num((yday + 7 - (wday + 6).rem_euclid(7)).div_euclid(7), 2, b'0'),
        b'y' => sink.num(year.rem_euclid(100), 2, b'0'),
        b'Y' => sink.num(year, 4, b'0'),
        b'z' => {
            let magnitude = gmtoff.unsigned_abs();
            sink.byte(if gmtoff < 0 { b'-' } else { b'+' })
                && sink.num((magnitude / 3600) as i64, 2, b'0')
                && sink.num((magnitude / 60 % 60) as i64, 2, b'0')
        }
        b'Z' => sink.bytes(zone.name()),
        b'%' => sink.byte(b'%'),
        _ => echo(sink, modifier, conv),
    }
}

/// An unrecognised conversion comes back out verbatim, which is what glibc
/// and musl do and what portable callers rely on.
fn echo(sink: &mut Sink<'_>, modifier: Option<u8>, conv: u8) -> bool {
    if !sink.byte(b'%') {
        return false;
    }
    if let Some(m) = modifier {
        if !sink.byte(m) {
            return false;
        }
    }
    sink.byte(conv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::utc_from_epoch;

    /// Sunday 2020-09-13 12:26:40 UTC.
    fn sample() -> Tm {
        utc_from_epoch(1_600_000_000).unwrap()
    }

    #[track_caller]
    fn check(fmt: &str, tm: &Tm, want: &str) {
        let mut buf = [0xAAu8; 256];
        let len = format(&mut buf, fmt.as_bytes(), tm, 0, b"UTC".as_slice()).expect(fmt);
        assert_eq!(core::str::from_utf8(&buf[..len]).unwrap(), want, "{fmt}");
        assert_eq!(buf[len], 0, "{fmt} left no NUL");
    }

    #[test]
    fn every_conversion_renders_the_c_locale_answer() {
        let tm = sample();
        let cases = [
            ("%a", "Sun"),
            ("%A", "Sunday"),
            ("%b", "Sep"),
            ("%h", "Sep"),
            ("%B", "September"),
            ("%c", "Sun Sep 13 12:26:40 2020"),
            ("%C", "20"),
            ("%d", "13"),
            ("%D", "09/13/20"),
            ("%e", "13"),
            ("%F", "2020-09-13"),
            ("%g", "20"),
            ("%G", "2020"),
            ("%H", "12"),
            ("%I", "12"),
            ("%j", "257"),
            ("%m", "09"),
            ("%M", "26"),
            ("%n", "\n"),
            ("%p", "PM"),
            ("%r", "12:26:40 PM"),
            ("%R", "12:26"),
            ("%s", "1600000000"),
            ("%S", "40"),
            ("%t", "\t"),
            ("%T", "12:26:40"),
            ("%u", "7"),
            ("%U", "37"),
            ("%V", "37"),
            ("%w", "0"),
            ("%W", "36"),
            ("%x", "09/13/20"),
            ("%X", "12:26:40"),
            ("%y", "20"),
            ("%Y", "2020"),
            ("%z", "+0000"),
            ("%Z", "UTC"),
            ("%%", "%"),
            ("[%Y-%m-%d %H:%M:%S]", "[2020-09-13 12:26:40]"),
        ];
        for (fmt, want) in cases {
            check(fmt, &tm, want);
        }
    }

    /// The first day of a year is where `%U` and `%W` separate: a truncating
    /// divide hides an off-by-one in the middle of a year.
    #[test]
    fn week_numbers_agree_with_the_c_locale_on_the_first_of_january() {
        let tm = utc_from_epoch(1_672_531_200).unwrap(); // Sun 2023-01-01
        check("%U|%W|%V|%G", &tm, "01|00|52|2022");

        let tm = utc_from_epoch(1_640_995_200).unwrap(); // Sat 2022-01-01
        check("%U|%W|%V|%G", &tm, "00|00|52|2021");
    }

    #[test]
    fn single_digit_fields_pad_the_way_each_conversion_says() {
        let tm = utc_from_epoch(946_689_845).unwrap(); // Sat 2000-01-01 01:24:05
        check("%d|%e|%j|%H|%I|%p|%m", &tm, "01| 1|001|01|01|AM|01");
        check("%c", &tm, "Sat Jan  1 01:24:05 2000");
    }

    #[test]
    fn noon_and_midnight_are_twelve_in_twelve_hour_time() {
        check("%I %p", &utc_from_epoch(0).unwrap(), "12 AM");
        check("%I %p", &utc_from_epoch(12 * 3600).unwrap(), "12 PM");
        check("%I %p", &utc_from_epoch(13 * 3600).unwrap(), "01 PM");
        check("%I %p", &utc_from_epoch(23 * 3600).unwrap(), "11 PM");
    }

    #[test]
    fn era_and_alternative_modifiers_fall_back_to_the_plain_conversion() {
        let tm = sample();
        check("%EY %Oy %Ec", &tm, "2020 20 Sun Sep 13 12:26:40 2020");
    }

    #[test]
    fn an_unknown_conversion_comes_back_verbatim() {
        let tm = sample();
        check("%Q", &tm, "%Q");
        check("%EQ", &tm, "%EQ");
        check("a%", &tm, "a%");
        check("%E", &tm, "%E");
        check("%5", &tm, "%5");
    }

    #[test]
    fn pre_epoch_and_wide_years_still_render() {
        let tm = utc_from_epoch(-2_208_988_800).unwrap(); // Mon 1900-01-01
        check("%c", &tm, "Mon Jan  1 00:00:00 1900");
        check("%s %C %y", &tm, "-2208988800 19 00");

        let mut tm = Tm {
            year: -1900,
            mon: 0,
            mday: 1,
            ..Tm::default()
        };
        epoch_from_tm(&mut tm).unwrap();
        check("%Y %C %F", &tm, "0000 00 0000-01-01");
    }

    #[test]
    fn the_output_must_fit_with_its_nul() {
        let tm = sample();
        let mut buf = [0xAAu8; 8];

        assert_eq!(
            format(&mut buf[..5], b"%Y", &tm, 0, b"UTC".as_slice()),
            Some(4)
        );
        assert_eq!(&buf[..5], b"2020\0");

        assert_eq!(
            format(&mut buf[..4], b"%Y", &tm, 0, b"UTC".as_slice()),
            None
        );
        assert_eq!(
            format(&mut buf[..1], b"", &tm, 0, b"UTC".as_slice()),
            Some(0)
        );
        assert_eq!(format(&mut buf[..0], b"", &tm, 0, b"UTC".as_slice()), None);
        assert_eq!(buf[5..], [0xAA; 3]);
    }

    #[test]
    fn a_truncated_expansion_fails_rather_than_overruns() {
        let tm = sample();
        let mut buf = [0xAAu8; 32];
        assert_eq!(
            format(&mut buf[..24], b"%c", &tm, 0, b"UTC".as_slice()),
            None
        );
        assert_eq!(
            format(&mut buf[..25], b"%c", &tm, 0, b"UTC".as_slice()),
            Some(24)
        );
        assert!(buf[25..].iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn an_offset_zone_renders_as_hours_and_minutes() {
        let tm = sample();
        let mut buf = [0u8; 16];
        let offset = -(5 * 3600 + 30 * 60);
        let len = format(&mut buf, b"%z %Z", &tm, offset, b"XYZ".as_slice()).unwrap();
        assert_eq!(&buf[..len], b"-0530 XYZ");
        let len = format(&mut buf, b"%z", &tm, 9 * 3600, b"UTC".as_slice()).unwrap();
        assert_eq!(&buf[..len], b"+0900");
    }

    #[test]
    fn asctime_has_the_fixed_twenty_five_column_shape() {
        let mut buf = [0u8; 26];
        assert!(asctime(&utc_from_epoch(0).unwrap(), &mut buf));
        assert_eq!(&buf, b"Thu Jan  1 00:00:00 1970\n\0");

        assert!(asctime(&utc_from_epoch(1_600_000_000).unwrap(), &mut buf));
        assert_eq!(&buf, b"Sun Sep 13 12:26:40 2020\n\0");

        assert!(asctime(&utc_from_epoch(946_689_845).unwrap(), &mut buf));
        assert_eq!(&buf, b"Sat Jan  1 01:24:05 2000\n\0");
    }

    #[test]
    fn asctime_refuses_a_year_that_does_not_fit() {
        let mut buf = [0u8; 26];

        let mut tm = Tm {
            year: 8100,
            mon: 0,
            mday: 1,
            ..Tm::default()
        };
        epoch_from_tm(&mut tm).unwrap();
        assert_eq!(tm.year + 1900, 10000);
        assert!(!asctime(&tm, &mut buf));

        let mut tm = Tm {
            year: -12000,
            mon: 0,
            mday: 1,
            ..Tm::default()
        };
        epoch_from_tm(&mut tm).unwrap();
        assert_eq!(tm.year + 1900, -10100);
        assert!(!asctime(&tm, &mut buf));

        // Four columns is the widest year that fits, its sign included.
        let mut tm = Tm {
            year: -2000,
            mon: 0,
            mday: 1,
            ..Tm::default()
        };
        epoch_from_tm(&mut tm).unwrap();
        assert!(asctime(&tm, &mut buf));
        assert_eq!(&buf, b"Mon Jan  1 00:00:00 -100\n\0");
    }
}
