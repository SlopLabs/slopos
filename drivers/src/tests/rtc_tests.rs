//! CMOS RTC decode and wall-clock anchoring.
//!
//! Nothing here reads live hardware except the one test that asserts the boot
//! step actually anchored `CLOCK_REALTIME` — a test that merely read the QEMU
//! RTC and asserted it was nonzero would pass on a broken decoder.

use slopos_kernel_services::clock;
use slopos_testing::{TestResult, fail, pass};

use crate::rtc::{RtcRaw, bcd_to_bin, days_from_civil, decode_unix_secs};

const STATUS_B_BCD_24H: u8 = 0x02;
const STATUS_B_BCD_12H: u8 = 0x00;
const STATUS_B_BIN_24H: u8 = 0x06;

static BCD_CASES: [(u8, Option<u8>); 8] = [
    (0x00, Some(0)),
    (0x09, Some(9)),
    (0x10, Some(10)),
    (0x59, Some(59)),
    (0x99, Some(99)),
    (0x0A, None),
    (0xA0, None),
    (0xFF, None),
];

/// `(year, month, day, days since 1970-01-01)`. Covers the epoch itself, a
/// leap day, both kinds of century boundary (2000 is a leap year, 2100 is
/// not), and a pre-epoch date so the negative-era branch is exercised.
static CIVIL_CASES: [(i64, u32, u32, i64); 6] = [
    (1970, 1, 1, 0),
    (2020, 1, 1, 18262),
    (2024, 2, 29, 19782),
    (2000, 3, 1, 11017),
    (2100, 3, 1, 47541),
    (1900, 3, 1, -25508),
];

/// Every one of these must decode to `None`: a `Some` is stamped into inodes.
static REJECT_CASES: [(&str, RtcRaw, u8); 8] = [
    (
        "month 13",
        RtcRaw {
            sec: 0x00,
            min: 0x00,
            hour: 0x00,
            day: 0x01,
            month: 0x13,
            year: 0x24,
            century: 0x20,
        },
        STATUS_B_BCD_24H,
    ),
    (
        "day 32",
        RtcRaw {
            sec: 0x00,
            min: 0x00,
            hour: 0x00,
            day: 0x32,
            month: 0x01,
            year: 0x24,
            century: 0x20,
        },
        STATUS_B_BCD_24H,
    ),
    (
        "31 February",
        RtcRaw {
            sec: 0x00,
            min: 0x00,
            hour: 0x00,
            day: 0x30,
            month: 0x02,
            year: 0x24,
            century: 0x20,
        },
        STATUS_B_BCD_24H,
    ),
    (
        "29 February in a non-leap year",
        RtcRaw {
            sec: 0x00,
            min: 0x00,
            hour: 0x00,
            day: 0x29,
            month: 0x02,
            year: 0x23,
            century: 0x20,
        },
        STATUS_B_BCD_24H,
    ),
    (
        "second 60",
        RtcRaw {
            sec: 0x60,
            min: 0x00,
            hour: 0x00,
            day: 0x01,
            month: 0x01,
            year: 0x24,
            century: 0x20,
        },
        STATUS_B_BCD_24H,
    ),
    (
        "non-BCD nibble in BCD mode",
        RtcRaw {
            sec: 0x1F,
            min: 0x00,
            hour: 0x00,
            day: 0x01,
            month: 0x01,
            year: 0x24,
            century: 0x20,
        },
        STATUS_B_BCD_24H,
    ),
    (
        "year before the sanity window",
        RtcRaw {
            sec: 0x00,
            min: 0x00,
            hour: 0x00,
            day: 0x01,
            month: 0x01,
            year: 0x19,
            century: 0x20,
        },
        STATUS_B_BCD_24H,
    ),
    (
        "year past the sanity window",
        RtcRaw {
            sec: 0x00,
            min: 0x00,
            hour: 0x00,
            day: 0x01,
            month: 0x01,
            year: 0x05,
            century: 0x21,
        },
        STATUS_B_BCD_24H,
    ),
];

pub fn test_rtc_bcd_decode() -> TestResult {
    for &(input, expected) in BCD_CASES.iter() {
        let got = bcd_to_bin(input);
        if got != expected {
            return fail!(
                "bcd_to_bin(0x{:02x}) = {:?}, want {:?}",
                input,
                got,
                expected
            );
        }
    }
    pass!()
}

pub fn test_rtc_days_from_civil() -> TestResult {
    for &(year, month, day, expected) in CIVIL_CASES.iter() {
        let got = days_from_civil(year, month, day);
        if got != expected {
            return fail!(
                "days_from_civil({}, {}, {}) = {}, want {}",
                year,
                month,
                day,
                got,
                expected
            );
        }
    }
    pass!()
}

pub fn test_rtc_decode_bcd_and_binary_agree() -> TestResult {
    // 2024-02-29T12:45:30Z
    const EXPECTED: u64 = 1_709_210_730;

    let bcd = RtcRaw {
        sec: 0x30,
        min: 0x45,
        hour: 0x12,
        day: 0x29,
        month: 0x02,
        year: 0x24,
        century: 0x20,
    };
    match decode_unix_secs(bcd, STATUS_B_BCD_24H) {
        Some(EXPECTED) => {}
        other => return fail!("BCD 24-hour decoded to {:?}, want {}", other, EXPECTED),
    }

    let binary = RtcRaw {
        sec: 30,
        min: 45,
        hour: 12,
        day: 29,
        month: 2,
        year: 24,
        century: 20,
    };
    match decode_unix_secs(binary, STATUS_B_BIN_24H) {
        Some(EXPECTED) => {}
        other => return fail!("binary 24-hour decoded to {:?}, want {}", other, EXPECTED),
    }

    // No century register: the two-digit year is read as 20xx, which must land
    // on the same date the century register names.
    let windowed = RtcRaw { century: 0, ..bcd };
    match decode_unix_secs(windowed, STATUS_B_BCD_24H) {
        Some(EXPECTED) => pass!(),
        other => fail!("windowed year decoded to {:?}, want {}", other, EXPECTED),
    }
}

pub fn test_rtc_decode_12_hour() -> TestResult {
    static CASES: [(u8, u64); 4] = [
        (0x12, 0),          // 12 AM is hour 0
        (0x01, 3_600),      // 1 AM
        (0x92, 12 * 3_600), // 12 PM is hour 12
        (0x81, 13 * 3_600), // 1 PM
    ];

    // 2024-02-29T00:00:00Z plus the hour under test.
    const MIDNIGHT: u64 = 1_709_164_800;

    for &(hour, offset) in CASES.iter() {
        let raw = RtcRaw {
            sec: 0x00,
            min: 0x00,
            hour,
            day: 0x29,
            month: 0x02,
            year: 0x24,
            century: 0x20,
        };
        let want = MIDNIGHT + offset;
        match decode_unix_secs(raw, STATUS_B_BCD_12H) {
            Some(got) if got == want => {}
            other => {
                return fail!(
                    "12-hour 0x{:02x} decoded to {:?}, want {}",
                    hour,
                    other,
                    want
                );
            }
        }
    }

    // Hour 0 does not exist in 12-hour mode; accepting it would silently
    // report midnight for a register the hardware never writes.
    let zero = RtcRaw {
        sec: 0x00,
        min: 0x00,
        hour: 0x00,
        day: 0x29,
        month: 0x02,
        year: 0x24,
        century: 0x20,
    };
    match decode_unix_secs(zero, STATUS_B_BCD_12H) {
        None => pass!(),
        Some(secs) => fail!("12-hour hour 0 decoded to {}", secs),
    }
}

pub fn test_rtc_decode_rejects_impossible_fields() -> TestResult {
    for &(label, raw, status_b) in REJECT_CASES.iter() {
        if let Some(secs) = decode_unix_secs(raw, status_b) {
            return fail!("{} decoded to {} instead of being rejected", label, secs);
        }
    }
    pass!()
}

pub fn test_set_realtime_rejects_and_does_not_move_the_clock() -> TestResult {
    static REJECTED: [(&str, i64, u32); 5] = [
        ("nanos at one second", 1_700_000_000, 1_000_000_000),
        ("nanos past one second", 1_700_000_000, u32::MAX),
        ("negative seconds", -1, 0),
        ("seconds overflowing the nanosecond anchor", i64::MAX, 0),
        (
            "seconds one past the representable anchor",
            18_446_744_074,
            0,
        ),
    ];

    let Some(before) = clock::realtime_ns() else {
        return fail!("wall clock unset; cannot tell a refusal from a no-op");
    };

    for &(label, secs, nanos) in REJECTED.iter() {
        if clock::set_realtime(secs, nanos).is_ok() {
            return fail!("set_realtime({}, {}) accepted: {}", secs, nanos, label);
        }
    }

    let Some(after) = clock::realtime_ns() else {
        return fail!("a refused set_realtime cleared the wall clock");
    };
    if after < before {
        return fail!(
            "a refused set_realtime moved the clock backwards: {} -> {}",
            before,
            after
        );
    }
    pass!()
}

pub fn test_wall_clock_anchored_at_boot() -> TestResult {
    let Some(first) = clock::realtime_ns() else {
        return fail!("CLOCK_REALTIME unset after boot; ext2 will stamp no mtime");
    };
    let Some(second) = clock::realtime_ns() else {
        return fail!("CLOCK_REALTIME became unset between two reads");
    };
    if second < first {
        return fail!("realtime_ns went backwards: {} -> {}", first, second);
    }

    let Some(secs) = clock::realtime_unix_secs() else {
        return fail!("realtime_unix_secs disagrees with realtime_ns");
    };
    let Some((ts_secs, ts_nanos)) = clock::realtime_timespec() else {
        return fail!("realtime_timespec disagrees with realtime_ns");
    };
    if ts_nanos >= 1_000_000_000 {
        return fail!("realtime_timespec nanos out of range: {}", ts_nanos);
    }
    if i64::from(secs) != ts_secs {
        return fail!(
            "realtime_unix_secs {} disagrees with realtime_timespec {}",
            secs,
            ts_secs
        );
    }
    pass!()
}

slopos_testing::stest!(name = test_rtc_bcd_decode, suite = rtc);
slopos_testing::stest!(name = test_rtc_days_from_civil, suite = rtc);
slopos_testing::stest!(name = test_rtc_decode_bcd_and_binary_agree, suite = rtc);
slopos_testing::stest!(name = test_rtc_decode_12_hour, suite = rtc);
slopos_testing::stest!(
    name = test_rtc_decode_rejects_impossible_fields,
    suite = rtc
);
slopos_testing::stest!(
    name = test_set_realtime_rejects_and_does_not_move_the_clock,
    suite = rtc
);
slopos_testing::stest!(name = test_wall_clock_anchored_at_boot, suite = rtc);
