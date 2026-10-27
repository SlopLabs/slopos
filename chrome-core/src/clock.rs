//! What the menu bar's clock and the lock and login screens say of the wall
//! clock, in UTC until SlopOS has time zones.

use alloc::format;
use alloc::string::String;

use slopos_slibc_core::calendar::{Tm, utc_from_epoch};

/// What the clock's panel says under the date.
pub const ZONE: &str = "The clock shows UTC";
/// What the clock's panel says before the wall clock is known.
pub const DATE_UNKNOWN: &str = "The date isn't known yet";

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

const MONTHS: [&str; 12] = [
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

/// A second on the wall clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WallTime {
    tm: Tm,
    minute: i64,
}

impl WallTime {
    /// `secs` since the epoch; `None` for a time no calendar year holds.
    pub fn from_unix(secs: i64) -> Option<WallTime> {
        Some(WallTime {
            tm: utc_from_epoch(secs)?,
            minute: secs.div_euclid(60),
        })
    }

    /// `14:05`.
    pub fn time(&self) -> String {
        format!("{:02}:{:02}", self.tm.hour, self.tm.min)
    }

    /// `Tuesday 27 October`.
    pub fn day(&self) -> String {
        format!(
            "{} {} {}",
            WEEKDAYS[self.tm.wday as usize], self.tm.mday, MONTHS[self.tm.mon as usize]
        )
    }

    /// `Tuesday 27 October 2026`.
    pub fn date(&self) -> String {
        format!("{} {}", self.day(), i64::from(self.tm.year) + 1900)
    }

    /// Changes exactly when [`time`](Self::time) does.
    pub fn minute(&self) -> i64 {
        self.minute
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHASE_START: i64 = 1_793_085_900;

    #[test]
    fn a_known_moment_reads_as_its_calendar_says() {
        let t = WallTime::from_unix(PHASE_START).unwrap();
        assert_eq!(t.time(), "07:25");
        assert_eq!(t.day(), "Tuesday 27 October");
        assert_eq!(t.date(), "Tuesday 27 October 2026");
    }

    #[test]
    fn the_epoch_and_a_leap_day_read_whole() {
        let epoch = WallTime::from_unix(0).unwrap();
        assert_eq!(
            (epoch.time().as_str(), epoch.date().as_str()),
            ("00:00", "Thursday 1 January 1970")
        );
        let leap = WallTime::from_unix(1_709_251_199).unwrap();
        assert_eq!(
            (leap.time().as_str(), leap.date().as_str()),
            ("23:59", "Thursday 29 February 2024")
        );
    }

    #[test]
    fn the_minute_moves_with_what_the_clock_shows() {
        let mut last = WallTime::from_unix(PHASE_START - 120).unwrap();
        for secs in PHASE_START - 119..PHASE_START + 3600 {
            let now = WallTime::from_unix(secs).unwrap();
            assert_eq!(
                now.minute() != last.minute(),
                now.time() != last.time(),
                "{secs}"
            );
            last = now;
        }
    }

    #[test]
    fn every_hour_and_minute_is_two_digits() {
        for secs in (0..86_400).step_by(60) {
            let shown = WallTime::from_unix(PHASE_START - PHASE_START % 86_400 + secs)
                .unwrap()
                .time();
            assert_eq!(shown.len(), 5, "{shown}");
            assert_eq!(&shown[2..3], ":");
        }
    }

    #[test]
    fn the_panel_s_words_speak_plainly() {
        use slopos_priv_core::words::label;
        for text in [ZONE, DATE_UNKNOWN] {
            assert_eq!(label(text), Ok(()), "{text}");
        }
    }
}
