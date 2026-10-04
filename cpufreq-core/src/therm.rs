//! The thermal status registers: whether a core or the package is held below
//! the frequency asked of it, and why, and how hot it is.
//!
//! `IA32_THERM_STATUS` (`0x19C`, per core) and `IA32_PACKAGE_THERM_STATUS`
//! (`0x1B1`) share the bits read here. Each condition has a status bit (now)
//! and a sticky log bit (since software last cleared it, which nothing here
//! does). The temperature is read as degrees below the throttling point that
//! `MSR_TEMPERATURE_TARGET` (`0x1A2`) bits 23:16 name.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThermStatus {
    /// Bits 0/1: at or above the thermal throttling point.
    pub thermal: Condition,
    /// Bits 2/3: PROCHOT asserted by another agent on the board.
    pub prochot: Condition,
    /// Bits 4/5: at the critical temperature.
    pub critical: Condition,
    /// Bits 10/11: held below the requested frequency by a power limit.
    pub power_limit: Condition,
    /// Bits 12/13: held below it by a current limit.
    pub current_limit: Condition,
    /// Bits 22:16 when bit 31 says the reading is valid.
    pub below_target: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Condition {
    pub now: bool,
    pub since_boot: bool,
}

impl Condition {
    const fn at(value: u64, bit: u32) -> Self {
        Self {
            now: value & (1 << bit) != 0,
            since_boot: value & (1 << (bit + 1)) != 0,
        }
    }
}

impl ThermStatus {
    pub const fn from_msr(value: u64) -> Self {
        Self {
            thermal: Condition::at(value, 0),
            prochot: Condition::at(value, 2),
            critical: Condition::at(value, 4),
            power_limit: Condition::at(value, 10),
            current_limit: Condition::at(value, 12),
            below_target: if value & (1 << 31) != 0 {
                Some(((value >> 16) & 0x7F) as u8)
            } else {
                None
            },
        }
    }

    /// Degrees Celsius, given `MSR_TEMPERATURE_TARGET`; `None` without a valid
    /// reading or target.
    pub const fn celsius(self, temperature_target: u64) -> Option<u8> {
        let target = throttle_point(temperature_target);
        match self.below_target {
            Some(below) if target != 0 => Some(target.saturating_sub(below)),
            _ => None,
        }
    }
}

/// `MSR_TEMPERATURE_TARGET` bits 23:16, the throttling point in °C.
pub const fn throttle_point(temperature_target: u64) -> u8 {
    (temperature_target >> 16) as u8
}
