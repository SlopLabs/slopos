//! Usage pages and the usages SlopOS acts on. A usage is `page << 16 | id`.

pub const fn usage(page: u16, id: u16) -> u32 {
    (page as u32) << 16 | id as u32
}

pub const fn page_of(usage: u32) -> u16 {
    (usage >> 16) as u16
}

pub const fn id_of(usage: u32) -> u16 {
    usage as u16
}

pub mod page {
    pub const GENERIC_DESKTOP: u16 = 0x01;
    pub const KEYBOARD: u16 = 0x07;
    pub const LED: u16 = 0x08;
    pub const BUTTON: u16 = 0x09;
    pub const CONSUMER: u16 = 0x0c;
    pub const DIGITIZER: u16 = 0x0d;
}

use page::*;

pub const POINTER: u32 = usage(GENERIC_DESKTOP, 0x01);
pub const MOUSE: u32 = usage(GENERIC_DESKTOP, 0x02);
pub const KEYBOARD: u32 = usage(GENERIC_DESKTOP, 0x06);
pub const KEYPAD: u32 = usage(GENERIC_DESKTOP, 0x07);
pub const X: u32 = usage(GENERIC_DESKTOP, 0x30);
pub const Y: u32 = usage(GENERIC_DESKTOP, 0x31);
pub const WHEEL: u32 = usage(GENERIC_DESKTOP, 0x38);
pub const AC_PAN: u32 = usage(CONSUMER, 0x0238);

pub const NUM_LOCK: u32 = usage(LED, 0x01);
pub const CAPS_LOCK: u32 = usage(LED, 0x02);
pub const SCROLL_LOCK: u32 = usage(LED, 0x03);

pub const TIP_SWITCH: u32 = usage(DIGITIZER, 0x42);
pub const CONTACT_ID: u32 = usage(DIGITIZER, 0x51);
pub const INPUT_MODE: u32 = usage(DIGITIZER, 0x52);

/// Keyboard page ids a report sends in place of keys: ErrorRollOver,
/// POSTFail and ErrorUndefined.
pub const KEY_ERRORS: core::ops::RangeInclusive<u16> = 0x01..=0x03;
pub const KEY_LEFT_CONTROL: u16 = 0xe0;
pub const KEY_RIGHT_META: u16 = 0xe7;
