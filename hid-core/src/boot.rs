//! The boot protocol's fixed reports (Appendix B), which a boot device sends
//! whatever its report descriptor says once `SET_PROTOCOL` selects them.

pub const KEYBOARD_LEN: usize = 8;
pub const MOUSE_LEN: usize = 3;
pub const KEYBOARD_KEYS: usize = 6;

/// The keyboard's LED output report, one bit per LED.
pub mod led {
    pub const NUM_LOCK: u8 = 1 << 0;
    pub const CAPS_LOCK: u8 = 1 << 1;
    pub const SCROLL_LOCK: u8 = 1 << 2;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keyboard {
    /// Left Control at bit 0 through Right GUI at bit 7.
    pub modifiers: u8,
    pub keys: [u8; KEYBOARD_KEYS],
}

impl Keyboard {
    /// `None` for a report shorter than the boot layout.
    pub fn parse(report: &[u8]) -> Option<Self> {
        let report = report.get(..KEYBOARD_LEN)?;
        let mut keys = [0; KEYBOARD_KEYS];
        keys.copy_from_slice(&report[2..]);
        Some(Self {
            modifiers: report[0],
            keys,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mouse {
    /// Buttons 1 to 3 at bits 0 to 2.
    pub buttons: u8,
    pub dx: i8,
    pub dy: i8,
}

impl Mouse {
    /// Bytes past the third are the device's own.
    pub fn parse(report: &[u8]) -> Option<Self> {
        let report = report.get(..MOUSE_LEN)?;
        Some(Self {
            buttons: report[0] & 0x07,
            dx: report[1] as i8,
            dy: report[2] as i8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_reports_take_their_fixed_bytes() {
        let kb = Keyboard::parse(&[0x22, 0xff, 4, 5, 0, 0, 0, 0, 0xee]).unwrap();
        assert_eq!(kb.modifiers, 0x22);
        assert_eq!(kb.keys, [4, 5, 0, 0, 0, 0]);
        assert_eq!(Keyboard::parse(&[0; 7]), None);
        let mouse = Mouse::parse(&[0xfd, 0x81, 0x7f, 0x05]).unwrap();
        assert_eq!(
            mouse,
            Mouse {
                buttons: 0x05,
                dx: -127,
                dy: 127
            }
        );
        assert_eq!(Mouse::parse(&[1, 2]), None);
    }
}
