//! MC146818-class CMOS real-time clock: the live wall-clock source.
//!
//! Read-only. Unlike the bootloader's one-shot hand-off value this can be
//! re-read, so it is what `CLOCK_REALTIME` is anchored to at boot.

use slopos_acpi::tables::AcpiTables;
use slopos_arch::cpu::interrupts::IrqDisabled;
use slopos_kernel_services::platform;
use slopos_ostd::io::CmosRegs;
use slopos_ostd::io::port::IoPortRegistry;
use slopos_ostd::klog_warn;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY_OF_MONTH: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;
/// Last register of the architectural RTC block; the century register, when
/// the platform has one, always sits above it.
const REG_STATUS_D: u8 = 0x0D;

/// Status A bit 7: an update cycle is in progress and the time registers are
/// undefined until it clears.
const STATUS_A_UIP: u8 = 0x80;
/// Status B bit 1: hours are 24-hour rather than 12-hour + PM flag.
const STATUS_B_24_HOUR: u8 = 0x02;
/// Status B bit 2: the time registers hold binary rather than packed BCD.
const STATUS_B_BINARY: u8 = 0x04;
/// Hours bit 7 in 12-hour mode: PM.
const HOUR_PM_FLAG: u8 = 0x80;

/// FADT offset of the CMOS century register's index, 0 when the platform has
/// none. ACPI 6.x, Table 5.9.
const FADT_OFF_CENTURY: usize = 108;

/// The update cycle lasts under 2 ms, so this is ample slack at legacy port-I/O
/// speed. A bound rather than a `while`: a machine with no RTC floats status A
/// to 0xFF, which reads as permanently updating.
const UIP_POLL_BUDGET: u32 = 1 << 16;

/// Two consecutive identical reads settle it; the retries cover an update
/// landing inside one of them.
const CONSISTENT_READ_ATTEMPTS: u32 = 8;

/// A date outside this half-open window is firmware garbage, not a clock. It
/// also closes the two-digit year when the platform advertises no century
/// register: that year is read as 20xx, and 2000..2020 fails the floor.
const MIN_UNIX_SECS: u64 = 1_577_836_800; // 2020-01-01T00:00:00Z
const MAX_UNIX_SECS: u64 = 4_102_444_800; // 2100-01-01T00:00:00Z

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RtcRaw {
    pub(crate) sec: u8,
    pub(crate) min: u8,
    pub(crate) hour: u8,
    pub(crate) day: u8,
    pub(crate) month: u8,
    pub(crate) year: u8,
    /// `0` when the platform advertises no century register.
    pub(crate) century: u8,
}

pub(crate) fn bcd_to_bin(value: u8) -> Option<u8> {
    let high = value >> 4;
    let low = value & 0x0F;
    if high > 9 || low > 9 {
        return None;
    }
    Some(high * 10 + low)
}

/// Days from 1970-01-01 to the proleptic-Gregorian `year-month-day`, by the
/// era/day-of-era decomposition rather than a month-length table.
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let shifted = if month > 2 { m - 3 } else { m + 9 };
    let doy = (153 * shifted + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// `None` for any field the hardware cannot legitimately hold, and for a date
/// outside the sanity window: a plausible wrong date is worse than none, which
/// every consumer treats as "stamp nothing".
pub(crate) fn decode_unix_secs(raw: RtcRaw, status_b: u8) -> Option<u64> {
    let binary = status_b & STATUS_B_BINARY != 0;
    let decode = |v: u8| if binary { Some(v) } else { bcd_to_bin(v) };

    let sec = decode(raw.sec)?;
    let min = decode(raw.min)?;
    let day = decode(raw.day)?;
    let month = decode(raw.month)?;
    let year_of_century = decode(raw.year)?;

    let pm = raw.hour & HOUR_PM_FLAG != 0;
    let mut hour = decode(raw.hour & !HOUR_PM_FLAG)?;
    if status_b & STATUS_B_24_HOUR == 0 {
        if hour == 0 || hour > 12 {
            return None;
        }
        hour %= 12;
        if pm {
            hour += 12;
        }
    } else if pm {
        return None;
    }

    let year = if raw.century != 0 {
        i64::from(decode(raw.century)?) * 100 + i64::from(year_of_century)
    } else {
        2000 + i64::from(year_of_century)
    };

    let month = u32::from(month);
    let day = u32::from(day);
    if sec > 59 || min > 59 || hour > 23 || !(1..=12).contains(&month) {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }

    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    let secs =
        (days as u64) * 86_400 + u64::from(hour) * 3_600 + u64::from(min) * 60 + u64::from(sec);
    if !(MIN_UNIX_SECS..MAX_UNIX_SECS).contains(&secs) {
        return None;
    }
    Some(secs)
}

/// Index of the CMOS century register, or `0` when the FADT names none.
fn fadt_century_register() -> u8 {
    if !platform::is_rsdp_available() {
        return 0;
    }
    let Some(tables) = AcpiTables::from_phys(platform::get_rsdp_phys()) else {
        return 0;
    };
    let Some(facp) = tables.find_table(b"FACP") else {
        return 0;
    };
    let reg = facp.raw().get(FADT_OFF_CENTURY).copied().unwrap_or(0);
    // Reject an index inside the architectural register block: status C is
    // read-to-clear, so a malformed FADT pointing there would eat the RTC's own
    // interrupt flags. Windowing the two-digit year is the cheaper failure.
    if reg <= REG_STATUS_D { 0 } else { reg }
}

fn read_reg(cmos: &CmosRegs, reg: u8) -> u8 {
    IrqDisabled::with(|irq| cmos.read(irq, reg))
}

fn read_raw(cmos: &CmosRegs, century_reg: u8) -> RtcRaw {
    IrqDisabled::with(|irq| RtcRaw {
        sec: cmos.read(irq, REG_SECONDS),
        min: cmos.read(irq, REG_MINUTES),
        hour: cmos.read(irq, REG_HOURS),
        day: cmos.read(irq, REG_DAY_OF_MONTH),
        month: cmos.read(irq, REG_MONTH),
        year: cmos.read(irq, REG_YEAR),
        century: if century_reg == 0 {
            0
        } else {
            cmos.read(irq, century_reg)
        },
    })
}

fn wait_update_done(cmos: &CmosRegs) -> bool {
    for _ in 0..UIP_POLL_BUDGET {
        if read_reg(cmos, REG_STATUS_A) & STATUS_A_UIP == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Seconds since the Unix epoch as the CMOS RTC reports them, or `None` when
/// the hardware is unreachable, will not settle, or names a date that cannot
/// be right.
pub fn rtc_read_unix_secs() -> Option<u64> {
    let (Ok(index), Ok(data)) = (
        IoPortRegistry::reserve::<u8>(0x70),
        IoPortRegistry::reserve::<u8>(0x71),
    ) else {
        klog_warn!("RTC: CMOS ports 0x70/0x71 are not reserved");
        return None;
    };
    let cmos = CmosRegs::new(index, data);
    let century_reg = fadt_century_register();

    if !wait_update_done(&cmos) {
        return None;
    }
    let status_b = read_reg(&cmos, REG_STATUS_B);
    let mut previous = read_raw(&cmos, century_reg);
    for _ in 0..CONSISTENT_READ_ATTEMPTS {
        if !wait_update_done(&cmos) {
            return None;
        }
        let current = read_raw(&cmos, century_reg);
        if current == previous {
            return decode_unix_secs(current, status_b);
        }
        previous = current;
    }
    None
}
