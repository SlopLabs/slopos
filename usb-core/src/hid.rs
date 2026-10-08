//! The HID class on USB (HID 1.11 §6.2.1, §7.2): the HID descriptor that
//! names the report descriptor's length, and the class requests. The reports
//! themselves are `hid-core`'s.

use crate::device::request::{
    DIRECTION_IN, GET_DESCRIPTOR, RECIPIENT_INTERFACE, Setup, TYPE_CLASS, TYPE_STANDARD,
};

pub const CLASS: u8 = 0x03;
pub const SUBCLASS_BOOT: u8 = 1;
pub const PROTOCOL_KEYBOARD: u8 = 1;
pub const PROTOCOL_MOUSE: u8 = 2;

pub const HID_DESCRIPTOR: u8 = 0x21;
pub const REPORT_DESCRIPTOR: u8 = 0x22;

const SET_REPORT: u8 = 0x09;
const SET_IDLE: u8 = 0x0a;
const SET_PROTOCOL: u8 = 0x0b;
const REPORT_OUTPUT: u16 = 2;

/// The report descriptor's length, from a HID descriptor's bytes; `None`
/// when they name no report descriptor or run short.
pub fn report_descriptor_length(bytes: &[u8]) -> Option<u16> {
    let len = usize::from(*bytes.first()?).min(bytes.len());
    if len < 6 || bytes[1] != HID_DESCRIPTOR {
        return None;
    }
    let count = usize::from(bytes[5]);
    bytes[6..len]
        .chunks_exact(3)
        .take(count)
        .find(|d| d[0] == REPORT_DESCRIPTOR)
        .map(|d| u16::from_le_bytes([d[1], d[2]]))
        .filter(|&length| length > 0)
}

impl Setup {
    pub fn get_report_descriptor(interface: u8, length: u16) -> Self {
        Self {
            request_type: DIRECTION_IN | TYPE_STANDARD | RECIPIENT_INTERFACE,
            request: GET_DESCRIPTOR,
            value: u16::from(REPORT_DESCRIPTOR) << 8,
            index: interface.into(),
            length,
        }
    }

    /// Boot protocol, or report protocol.
    pub fn set_protocol(interface: u8, boot: bool) -> Self {
        Self {
            request_type: TYPE_CLASS | RECIPIENT_INTERFACE,
            request: SET_PROTOCOL,
            value: u16::from(!boot),
            index: interface.into(),
            length: 0,
        }
    }

    /// Reports only on change, for every report.
    pub fn set_idle_forever(interface: u8) -> Self {
        Self {
            request_type: TYPE_CLASS | RECIPIENT_INTERFACE,
            request: SET_IDLE,
            value: 0,
            index: interface.into(),
            length: 0,
        }
    }

    pub fn set_output_report(interface: u8, id: u8, length: u16) -> Self {
        Self {
            request_type: TYPE_CLASS | RECIPIENT_INTERFACE,
            request: SET_REPORT,
            value: REPORT_OUTPUT << 8 | u16::from(id),
            index: interface.into(),
            length,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_descriptor_length_is_found_among_the_descriptors() {
        assert_eq!(
            report_descriptor_length(&[9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0]),
            Some(63)
        );
        let two = [12, 0x21, 0x11, 1, 0, 2, 0x23, 9, 0, 0x22, 0x34, 0x12];
        assert_eq!(report_descriptor_length(&two), Some(0x1234));
        assert_eq!(report_descriptor_length(&two[..11]), None, "cut short");
        assert_eq!(
            report_descriptor_length(&[9, 0x21, 0, 0, 0, 0, 0x22, 9, 0]),
            None
        );
        assert_eq!(
            report_descriptor_length(&[9, 0x22, 0, 0, 0, 1, 0x22, 9, 0]),
            None
        );
        assert_eq!(
            report_descriptor_length(&[9, 0x21, 0, 0, 0, 1, 0x22, 0, 0]),
            None
        );
        assert_eq!(report_descriptor_length(&[]), None);
    }

    #[test]
    fn class_requests_lay_out_as_hid_names_them() {
        assert_eq!(
            Setup::get_report_descriptor(2, 63).bytes(),
            [0x81, 6, 0, 0x22, 2, 0, 63, 0]
        );
        assert_eq!(
            Setup::set_protocol(1, true).bytes(),
            [0x21, 0x0b, 0, 0, 1, 0, 0, 0]
        );
        assert_eq!(Setup::set_protocol(1, false).value, 1);
        assert_eq!(
            Setup::set_idle_forever(0).bytes(),
            [0x21, 0x0a, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            Setup::set_output_report(0, 5, 2).bytes(),
            [0x21, 0x09, 5, 2, 0, 0, 2, 0]
        );
    }

    #[test]
    fn mutated_hid_descriptors_never_panic() {
        let bytes = [12u8, 0x21, 0x11, 1, 0, 2, 0x23, 9, 0, 0x22, 0x34, 0x12];
        let mut state = 0x1234_5678_9abc_def1u64;
        for at in 0..bytes.len() {
            for _ in 0..32 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let mut mutated = bytes;
                mutated[at] = state as u8;
                let cut = (state >> 8) as usize % (bytes.len() + 1);
                let _ = report_descriptor_length(&mutated[..cut]);
                let _ = report_descriptor_length(&mutated);
            }
        }
    }
}
