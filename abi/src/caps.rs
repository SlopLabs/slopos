//! The bits of a capability mask, as `caps_drop` takes and answers one.
//! `slopos_ostd::authority::Capability` says what each one gates.

pub const POWER: u64 = 1 << 0;
pub const LAUNCH: u64 = 1 << 1;
pub const PROC_SIGNAL: u64 = 1 << 2;
pub const SYS_INSPECT: u64 = 1 << 3;
pub const DISPLAY_SEAT: u64 = 1 << 4;
pub const INPUT_SEAT: u64 = 1 << 5;
pub const CONSOLE_CONFIG: u64 = 1 << 6;
pub const CONSOLE_IO: u64 = 1 << 7;
pub const CLIPBOARD_GLOBAL: u64 = 1 << 8;
pub const FATE: u64 = 1 << 9;
pub const TEST_HARNESS: u64 = 1 << 10;
pub const MOUNT: u64 = 1 << 11;
pub const CLOCK: u64 = 1 << 12;
pub const SEAL: u64 = 1 << 13;
pub const BOOT_ENTRY: u64 = 1 << 14;
pub const NET_ADMIN: u64 = 1 << 15;
pub const RAW_BLOCK: u64 = 1 << 16;
pub const SET_ID: u64 = 1 << 17;
pub const DAC_OVERRIDE: u64 = 1 << 18;

/// What every program holds.
pub const UNIVERSAL: u64 = CONSOLE_IO | CLIPBOARD_GLOBAL | FATE;

/// Every bit a capability names.
pub const ALL: u64 = (DAC_OVERRIDE << 1) - 1;
