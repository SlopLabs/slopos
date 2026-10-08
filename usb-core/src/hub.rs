//! Hubs (USB 2.0 chapter 11, USB 3.2 chapter 10), and the port status root
//! and hub ports share.

use crate::device::Speed;
use crate::device::descriptor::Malformed;
use crate::device::request::{
    CLEAR_FEATURE, DIRECTION_IN, GET_DESCRIPTOR, GET_STATUS, RECIPIENT_DEVICE, RECIPIENT_OTHER,
    SET_FEATURE, Setup, TYPE_CLASS,
};

pub const HUB_DESCRIPTOR: u8 = 0x29;
pub const SUPERSPEED_HUB_DESCRIPTOR: u8 = 0x2a;
const CLEAR_TT_BUFFER: u8 = 8;
const SET_HUB_DEPTH: u8 = 12;

/// A route string holds port numbers up to 15.
pub const MAX_HUB_PORTS: u8 = 15;

/// A route string holds five tiers of hubs below a root port.
pub const MAX_DEPTH: u8 = 5;

/// Port features, USB 2.0 Table 11-17 and USB 3.2 Table 10-10.
pub mod feature {
    pub const PORT_ENABLE: u16 = 1;
    pub const PORT_RESET: u16 = 4;
    pub const PORT_POWER: u16 = 8;
    pub const C_PORT_CONNECTION: u16 = 16;
    pub const C_PORT_ENABLE: u16 = 17;
    pub const C_PORT_SUSPEND: u16 = 18;
    pub const C_PORT_OVER_CURRENT: u16 = 19;
    pub const C_PORT_RESET: u16 = 20;
    pub const C_PORT_LINK_STATE: u16 = 25;
    pub const C_PORT_CONFIG_ERROR: u16 = 26;
    pub const C_BH_PORT_RESET: u16 = 29;
    pub const C_HUB_LOCAL_POWER: u16 = 0;
    pub const C_HUB_OVER_CURRENT: u16 = 1;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HubDescriptor {
    pub ports: u8,
    pub characteristics: u16,
    pub power_good_ms: u32,
}

impl HubDescriptor {
    pub fn parse(bytes: &[u8], super_speed: bool) -> Result<Self, Malformed> {
        let (kind, len) = if super_speed {
            (SUPERSPEED_HUB_DESCRIPTOR, 12)
        } else {
            (HUB_DESCRIPTOR, 7)
        };
        if bytes.len() < len {
            return Err(Malformed::Short);
        }
        if bytes[1] != kind {
            return Err(Malformed::Kind);
        }
        if usize::from(bytes[0]) < len {
            return Err(Malformed::Length);
        }
        Ok(Self {
            ports: bytes[2],
            characteristics: u16::from_le_bytes([bytes[3], bytes[4]]),
            power_good_ms: u32::from(bytes[5]) * 2,
        })
    }

    /// The TT think time in the slot context's encoding: 8 to 32 full-speed
    /// bit times as 0 to 3.
    pub fn think_time(&self) -> u8 {
        ((self.characteristics >> 5) & 0b11) as u8
    }
}

/// The fixed part and both bitmaps of a hub of [`MAX_HUB_PORTS`].
pub fn descriptor_length(super_speed: bool) -> u16 {
    if super_speed { 12 } else { 11 }
}

pub fn get_descriptor(super_speed: bool) -> Setup {
    let kind = if super_speed {
        SUPERSPEED_HUB_DESCRIPTOR
    } else {
        HUB_DESCRIPTOR
    };
    Setup {
        request_type: DIRECTION_IN | TYPE_CLASS | RECIPIENT_DEVICE,
        request: GET_DESCRIPTOR,
        value: u16::from(kind) << 8,
        index: 0,
        length: descriptor_length(super_speed),
    }
}

pub fn get_hub_status() -> Setup {
    Setup {
        request_type: DIRECTION_IN | TYPE_CLASS | RECIPIENT_DEVICE,
        request: GET_STATUS,
        value: 0,
        index: 0,
        length: 4,
    }
}

pub fn clear_hub_feature(feature: u16) -> Setup {
    Setup {
        request_type: TYPE_CLASS | RECIPIENT_DEVICE,
        request: CLEAR_FEATURE,
        value: feature,
        index: 0,
        length: 0,
    }
}

pub fn get_port_status(port: u8) -> Setup {
    Setup {
        request_type: DIRECTION_IN | TYPE_CLASS | RECIPIENT_OTHER,
        request: GET_STATUS,
        value: 0,
        index: port.into(),
        length: 4,
    }
}

pub fn set_port_feature(port: u8, feature: u16) -> Setup {
    Setup {
        request_type: TYPE_CLASS | RECIPIENT_OTHER,
        request: SET_FEATURE,
        value: feature,
        index: port.into(),
        length: 0,
    }
}

pub fn clear_port_feature(port: u8, feature: u16) -> Setup {
    Setup {
        request_type: TYPE_CLASS | RECIPIENT_OTHER,
        request: CLEAR_FEATURE,
        value: feature,
        index: port.into(),
        length: 0,
    }
}

pub const TT_BULK: u8 = 2;

/// Drop what a transaction translator holds for the endpoint at `endpoint`
/// of the device at `address` (USB 2.0 §11.24.2.3); `tt_port` is 1 for a hub
/// run with one translator.
pub fn clear_tt_buffer(address: u8, endpoint: u8, endpoint_type: u8, tt_port: u16) -> Setup {
    let direction_in = u16::from(endpoint & 0x80 != 0);
    Setup {
        request_type: TYPE_CLASS | RECIPIENT_OTHER,
        request: CLEAR_TT_BUFFER,
        value: u16::from(endpoint & 0x0f)
            | u16::from(address & 0x7f) << 4
            | u16::from(endpoint_type & 3) << 11
            | direction_in << 15,
        index: tt_port,
        length: 0,
    }
}

/// A SuperSpeed hub's tier below the root port: 0 for a hub on a root port.
pub fn set_hub_depth(depth: u8) -> Setup {
    Setup {
        request_type: TYPE_CLASS | RECIPIENT_DEVICE,
        request: SET_HUB_DEPTH,
        value: depth.into(),
        index: 0,
        length: 0,
    }
}

pub mod change {
    pub const CONNECT: u16 = 1 << 0;
    pub const ENABLE: u16 = 1 << 1;
    pub const SUSPEND: u16 = 1 << 2;
    pub const OVER_CURRENT: u16 = 1 << 3;
    pub const RESET: u16 = 1 << 4;
    pub const WARM_RESET: u16 = 1 << 5;
    pub const LINK: u16 = 1 << 6;
    pub const CONFIG_ERROR: u16 = 1 << 7;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortStatus {
    pub connected: bool,
    pub enabled: bool,
    pub resetting: bool,
    pub over_current: bool,
    pub powered: bool,
    pub speed: Option<Speed>,
    /// [`change`] bits.
    pub changes: u16,
}

impl PortStatus {
    pub fn changed(&self, bits: u16) -> bool {
        self.changes & bits != 0
    }

    /// `wPortStatus` and `wPortChange`: a USB 2 hub leaves full speed implied,
    /// a SuperSpeed hub's ports run at SuperSpeed only.
    pub fn from_hub(bytes: [u8; 4], super_speed: bool) -> Self {
        let status = u16::from_le_bytes([bytes[0], bytes[1]]);
        let changed = u16::from_le_bytes([bytes[2], bytes[3]]);
        let bit = |word: u16, n: u32| word & 1 << n != 0;
        let connected = bit(status, 0);
        let (powered, speed) = if super_speed {
            (bit(status, 9), connected.then_some(Speed::Super))
        } else {
            let speed = match (bit(status, 9), bit(status, 10)) {
                (true, _) => Speed::Low,
                (false, true) => Speed::High,
                (false, false) => Speed::Full,
            };
            (bit(status, 8), connected.then_some(speed))
        };
        let mut changes = 0;
        let map: &[(u32, u16)] = if super_speed {
            &[
                (0, change::CONNECT),
                (3, change::OVER_CURRENT),
                (4, change::RESET),
                (5, change::WARM_RESET),
                (6, change::LINK),
                (7, change::CONFIG_ERROR),
            ]
        } else {
            &[
                (0, change::CONNECT),
                (1, change::ENABLE),
                (2, change::SUSPEND),
                (3, change::OVER_CURRENT),
                (4, change::RESET),
            ]
        };
        for &(n, flag) in map {
            if bit(changed, n) {
                changes |= flag;
            }
        }
        Self {
            connected,
            enabled: bit(status, 1),
            resetting: bit(status, 4),
            over_current: bit(status, 3),
            powered,
            speed,
            changes,
        }
    }
}

/// The feature that clears each bit of `changes`.
pub fn change_features(changes: u16, super_speed: bool) -> impl Iterator<Item = u16> {
    const USB2: u8 = 1;
    const USB3: u8 = 2;
    let table: [(u16, u16, u8); 8] = [
        (change::CONNECT, feature::C_PORT_CONNECTION, USB2 | USB3),
        (change::ENABLE, feature::C_PORT_ENABLE, USB2),
        (change::SUSPEND, feature::C_PORT_SUSPEND, USB2),
        (
            change::OVER_CURRENT,
            feature::C_PORT_OVER_CURRENT,
            USB2 | USB3,
        ),
        (change::RESET, feature::C_PORT_RESET, USB2 | USB3),
        (change::WARM_RESET, feature::C_BH_PORT_RESET, USB3),
        (change::LINK, feature::C_PORT_LINK_STATE, USB3),
        (change::CONFIG_ERROR, feature::C_PORT_CONFIG_ERROR, USB3),
    ];
    let generation = if super_speed { USB3 } else { USB2 };
    table
        .into_iter()
        .filter(move |&(bit, _, on)| changes & bit != 0 && on & generation != 0)
        .map(|(_, feature, _)| feature)
}

/// The ports a status-change report names, bit `p` for port `p`; bit 0 is
/// the hub itself.
pub fn changed(report: &[u8], ports: u8) -> u16 {
    let mut out = 0u16;
    for port in 0..=ports.min(MAX_HUB_PORTS) {
        let byte = usize::from(port / 8);
        if report.get(byte).is_some_and(|b| b & 1 << (port % 8) != 0) {
            out |= 1 << port;
        }
    }
    out
}

pub fn report_length(ports: u8) -> u16 {
    (u16::from(ports) + 1).div_ceil(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_of_both_generations_parse() {
        let usb2 = [9, 0x29, 4, 0x09, 0x00, 50, 100, 0, 0xff];
        let d = HubDescriptor::parse(&usb2, false).unwrap();
        assert_eq!((d.ports, d.power_good_ms), (4, 100));
        assert_eq!(d.think_time(), 0);
        let tt = [9, 0x29, 7, 0x69, 0x00, 1, 0, 0, 0xff];
        assert_eq!(HubDescriptor::parse(&tt, false).unwrap().think_time(), 3);
        let usb3 = [12, 0x2a, 4, 0x09, 0x00, 25, 0, 0, 0, 0, 0, 0];
        assert_eq!(HubDescriptor::parse(&usb3, true).unwrap().power_good_ms, 50);
        assert_eq!(HubDescriptor::parse(&usb3, false), Err(Malformed::Kind));
        assert_eq!(HubDescriptor::parse(&usb2, true), Err(Malformed::Short));
        assert_eq!(
            HubDescriptor::parse(&usb2[..6], false),
            Err(Malformed::Short)
        );
        let mut short = usb2;
        short[0] = 6;
        assert_eq!(HubDescriptor::parse(&short, false), Err(Malformed::Length));
    }

    #[test]
    fn requests_name_the_hub_or_a_port() {
        assert_eq!(
            get_descriptor(false).bytes(),
            [0xa0, 6, 0, 0x29, 0, 0, 11, 0]
        );
        assert_eq!(
            get_descriptor(true).bytes(),
            [0xa0, 6, 0, 0x2a, 0, 0, 12, 0]
        );
        assert_eq!(get_port_status(3).bytes(), [0xa3, 0, 0, 0, 3, 0, 4, 0]);
        assert_eq!(
            set_port_feature(2, feature::PORT_RESET).bytes(),
            [0x23, 3, 4, 0, 2, 0, 0, 0]
        );
        assert_eq!(
            clear_port_feature(2, feature::C_PORT_CONNECTION).bytes(),
            [0x23, 1, 16, 0, 2, 0, 0, 0]
        );
        assert_eq!(set_hub_depth(1).bytes(), [0x20, 12, 1, 0, 0, 0, 0, 0]);
        assert_eq!(get_hub_status().bytes(), [0xa0, 0, 0, 0, 0, 0, 4, 0]);
        assert_eq!(
            clear_hub_feature(feature::C_HUB_OVER_CURRENT).bytes(),
            [0x20, 1, 1, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn usb2_port_status_names_speed_by_its_two_bits() {
        let low = PortStatus::from_hub([0x03, 0x03, 0x11, 0x00], false);
        assert!(low.connected && low.enabled && low.powered);
        assert_eq!(low.speed, Some(Speed::Low));
        assert_eq!(low.changes, change::CONNECT | change::RESET);
        let high = PortStatus::from_hub([0x03, 0x05, 0, 0], false);
        assert_eq!(high.speed, Some(Speed::High));
        let full = PortStatus::from_hub([0x01, 0x01, 0x02, 0], false);
        assert_eq!(full.speed, Some(Speed::Full));
        assert_eq!(full.changes, change::ENABLE);
        let empty = PortStatus::from_hub([0x00, 0x01, 0x01, 0], false);
        assert_eq!(empty.speed, None);
        assert!(empty.powered && empty.changed(change::CONNECT));
        let resetting = PortStatus::from_hub([0x11, 0x01, 0, 0], false);
        assert!(resetting.resetting);
    }

    #[test]
    fn usb3_port_status_moves_power_and_adds_link_changes() {
        let s = PortStatus::from_hub([0x03, 0x02, 0xf1, 0x00], true);
        assert!(s.connected && s.enabled && s.powered);
        assert_eq!(s.speed, Some(Speed::Super));
        assert_eq!(
            s.changes,
            change::CONNECT
                | change::RESET
                | change::WARM_RESET
                | change::LINK
                | change::CONFIG_ERROR
        );
        let usb2_power_bit = PortStatus::from_hub([0x00, 0x01, 0, 0], true);
        assert!(!usb2_power_bit.powered);
    }

    #[test]
    fn each_change_is_cleared_by_its_own_feature() {
        let all = 0xff;
        let usb2: std::vec::Vec<u16> = change_features(all, false).collect();
        assert_eq!(usb2, [16, 17, 18, 19, 20]);
        let usb3: std::vec::Vec<u16> = change_features(all, true).collect();
        assert_eq!(
            usb3,
            [16, 19, 20, 29, 25, 26],
            "a SuperSpeed hub has no enable or suspend change, a USB 2 hub no link"
        );
        assert_eq!(change_features(0, false).count(), 0);
    }

    #[test]
    fn status_change_reports_name_ports_by_bit() {
        assert_eq!(changed(&[0b0000_0110], 4), 0b110);
        assert_eq!(changed(&[0x01, 0x80], 15), 0x8001);
        assert_eq!(
            changed(&[0xff], 3),
            0b1111,
            "bits past the ports are ignored"
        );
        assert_eq!(changed(&[], 4), 0);
        assert_eq!(changed(&[0, 0, 0xff], 15), 0);
        assert_eq!(report_length(4), 1);
        assert_eq!(report_length(7), 1);
        assert_eq!(report_length(8), 2);
        assert_eq!(report_length(15), 2);
    }
}
