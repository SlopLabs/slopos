//! Control requests: the eight-byte Setup packet and the standard requests
//! (USB 2.0 §9.3, §9.4).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

pub const DIRECTION_IN: u8 = 0x80;
pub const TYPE_STANDARD: u8 = 0x00;
pub const TYPE_CLASS: u8 = 0x20;
pub const TYPE_VENDOR: u8 = 0x40;
pub const RECIPIENT_DEVICE: u8 = 0x00;
pub const RECIPIENT_INTERFACE: u8 = 0x01;
pub const RECIPIENT_ENDPOINT: u8 = 0x02;
pub const RECIPIENT_OTHER: u8 = 0x03;

pub const GET_STATUS: u8 = 0;
pub const CLEAR_FEATURE: u8 = 1;
pub const SET_FEATURE: u8 = 3;
pub const GET_DESCRIPTOR: u8 = 6;
pub const SET_CONFIGURATION: u8 = 9;
pub const SET_INTERFACE: u8 = 11;

pub const ENDPOINT_HALT: u16 = 0;

pub const STATUS_SELF_POWERED: u16 = 1 << 0;

impl Setup {
    pub fn bytes(&self) -> [u8; 8] {
        let [value_lo, value_hi] = self.value.to_le_bytes();
        let [index_lo, index_hi] = self.index.to_le_bytes();
        let [length_lo, length_hi] = self.length.to_le_bytes();
        [
            self.request_type,
            self.request,
            value_lo,
            value_hi,
            index_lo,
            index_hi,
            length_lo,
            length_hi,
        ]
    }

    pub fn is_in(&self) -> bool {
        self.request_type & DIRECTION_IN != 0
    }

    pub fn get_descriptor(kind: u8, index: u8, language: u16, length: u16) -> Self {
        Self {
            request_type: DIRECTION_IN | TYPE_STANDARD | RECIPIENT_DEVICE,
            request: GET_DESCRIPTOR,
            value: u16::from(kind) << 8 | u16::from(index),
            index: language,
            length,
        }
    }

    pub fn set_configuration(value: u8) -> Self {
        Self {
            request_type: TYPE_STANDARD | RECIPIENT_DEVICE,
            request: SET_CONFIGURATION,
            value: value.into(),
            index: 0,
            length: 0,
        }
    }

    pub fn get_status() -> Self {
        Self {
            request_type: DIRECTION_IN | TYPE_STANDARD | RECIPIENT_DEVICE,
            request: GET_STATUS,
            value: 0,
            index: 0,
            length: 2,
        }
    }

    pub fn clear_halt(endpoint_address: u8) -> Self {
        Self {
            request_type: TYPE_STANDARD | RECIPIENT_ENDPOINT,
            request: CLEAR_FEATURE,
            value: ENDPOINT_HALT,
            index: endpoint_address.into(),
            length: 0,
        }
    }

    pub fn set_interface(interface: u8, alternate: u8) -> Self {
        Self {
            request_type: TYPE_STANDARD | RECIPIENT_INTERFACE,
            request: SET_INTERFACE,
            value: alternate.into(),
            index: interface.into(),
            length: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::descriptor::kind;

    #[test]
    fn requests_lay_out_little_endian() {
        let get = Setup::get_descriptor(kind::CONFIGURATION, 1, 0, 9);
        assert_eq!(get.bytes(), [0x80, 6, 1, 2, 0, 0, 9, 0]);
        assert!(get.is_in());
        let string = Setup::get_descriptor(kind::STRING, 2, 0x0409, 255);
        assert_eq!(string.bytes(), [0x80, 6, 2, 3, 0x09, 0x04, 255, 0]);
        assert_eq!(
            Setup::set_configuration(1).bytes(),
            [0, 9, 1, 0, 0, 0, 0, 0]
        );
        assert!(!Setup::set_configuration(1).is_in());
        assert_eq!(Setup::get_status().bytes(), [0x80, 0, 0, 0, 0, 0, 2, 0]);
        assert_eq!(Setup::clear_halt(0x81).bytes(), [2, 1, 0, 0, 0x81, 0, 0, 0]);
        assert_eq!(
            Setup::set_interface(2, 1).bytes(),
            [1, 11, 1, 0, 2, 0, 0, 0]
        );
    }
}
