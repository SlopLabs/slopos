//! Ethernet over USB's communications class: the ECM and NCM functions' descriptors,
//! class requests and notifications (CDC 1.2, ECM 1.2, NCM 1.0). The NTB
//! framing NCM moves frames in is [`ntb`]'s.

pub mod ntb;

use crate::device::descriptor::{Configuration, Endpoint, Item, TransferType, kind};
use crate::device::request::{DIRECTION_IN, RECIPIENT_INTERFACE, Setup, TYPE_CLASS};

pub const CLASS: u8 = 0x02;
pub const SUBCLASS_ECM: u8 = 0x06;
pub const SUBCLASS_NCM: u8 = 0x0d;
pub const DATA_CLASS: u8 = 0x0a;
/// The frame the stack sends and takes: a 1500-byte MTU, no CRC.
pub const MAX_FRAME: usize = 1514;
pub const ETHERNET_HEADER: usize = 14;

const CS_INTERFACE: u8 = 0x24;
const UNION: u8 = 0x06;
const ETHERNET: u8 = 0x0f;
const NCM: u8 = 0x1a;
const ETHERNET_LEN: usize = 13;
const NCM_LEN: usize = 6;

const SET_ETHERNET_PACKET_FILTER: u8 = 0x43;
const GET_NTB_PARAMETERS: u8 = 0x80;
const SET_NTB_FORMAT: u8 = 0x84;
const SET_NTB_INPUT_SIZE: u8 = 0x86;
const SET_CRC_MODE: u8 = 0x8a;

const NETWORK_CONNECTION: u8 = 0x00;
const NOTIFICATION_LEN: usize = 8;

/// `SET_ETHERNET_PACKET_FILTER`'s bits (ECM 1.2 §6.2.4).
pub mod filter {
    pub const PROMISCUOUS: u16 = 1 << 0;
    pub const ALL_MULTICAST: u16 = 1 << 1;
    pub const DIRECTED: u16 = 1 << 2;
    pub const BROADCAST: u16 = 1 << 3;
    pub const MULTICAST: u16 = 1 << 4;
}

/// The NCM functional descriptor's `bmNetworkCapabilities` (NCM 1.0 §5.2.1).
pub mod capability {
    pub const PACKET_FILTER: u8 = 1 << 0;
    pub const NET_ADDRESS: u8 = 1 << 1;
    pub const ENCAPSULATED_COMMAND: u8 = 1 << 2;
    pub const MAX_DATAGRAM_SIZE: u8 = 1 << 3;
    pub const CRC_MODE: u8 = 1 << 4;
    /// `SET_NTB_INPUT_SIZE` takes eight bytes, not four.
    pub const NTB_INPUT_SIZE_8: u8 = 1 << 5;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    Ecm,
    Ncm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Function {
    pub model: Model,
    pub control: u8,
    /// The union's first subordinate interface.
    pub data: u8,
    /// The data interface's first alternate setting with a bulk IN and a bulk
    /// OUT endpoint.
    pub alternate: u8,
    pub bulk_in: Endpoint,
    pub bulk_out: Endpoint,
    /// The interrupt IN endpoint of the control interface's alternate 0.
    pub notify: Option<Endpoint>,
    /// `iMACAddress`.
    pub mac_string: u8,
    /// `wMaxSegmentSize`.
    pub max_segment: u16,
    /// `bmNetworkCapabilities`, 0 for ECM.
    pub capabilities: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Model,
    Union,
    Data,
    Bulk,
    Ethernet,
    Segment(u16),
    Ncm,
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Model => f.write_str("not an ECM or NCM control interface"),
            Self::Union => f.write_str("no union naming a data interface"),
            Self::Data => f.write_str("no data class interface"),
            Self::Bulk => f.write_str("no data setting with bulk IN and OUT"),
            Self::Ethernet => f.write_str("no Ethernet descriptor with a MAC string"),
            Self::Segment(size) => write!(f, "maximum segment {size} under {MAX_FRAME}"),
            Self::Ncm => f.write_str("no NCM functional descriptor"),
        }
    }
}

fn functional(item: Item<'_>, subtype: u8, len: usize) -> Option<&[u8]> {
    match item {
        Item::Other {
            kind: CS_INTERFACE,
            bytes,
        } if bytes.len() >= len && bytes[2] == subtype => Some(bytes),
        _ => None,
    }
}

fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

impl Function {
    /// The ECM or NCM function whose control interface is `control`.
    pub fn parse(config: &Configuration<'_>, control: u8) -> Result<Self, Refusal> {
        let interface = config
            .interfaces()
            .find(|i| i.number == control && i.alternate == 0)
            .ok_or(Refusal::Model)?;
        let model = match (interface.class, interface.subclass) {
            (CLASS, SUBCLASS_ECM) => Model::Ecm,
            (CLASS, SUBCLASS_NCM) => Model::Ncm,
            _ => return Err(Refusal::Model),
        };
        let data = config
            .setting(control, 0)
            .filter_map(|item| functional(item, UNION, 5))
            .find(|union| union[3] == control)
            .map(|union| union[4])
            .ok_or(Refusal::Union)?;
        if !config
            .interfaces()
            .any(|i| i.number == data && i.alternate == 0 && i.class == DATA_CLASS)
        {
            return Err(Refusal::Data);
        }
        let (alternate, bulk_in, bulk_out) = config
            .interfaces()
            .filter(|i| i.number == data && i.class == DATA_CLASS)
            .find_map(|i| {
                let bulk = |is_in: bool| {
                    config
                        .endpoints(data, i.alternate)
                        .find(|e| e.transfer_type() == TransferType::Bulk && e.is_in() == is_in)
                };
                Some((i.alternate, bulk(true)?, bulk(false)?))
            })
            .ok_or(Refusal::Bulk)?;
        let ethernet = config
            .setting(control, 0)
            .find_map(|item| functional(item, ETHERNET, ETHERNET_LEN))
            .filter(|ethernet| ethernet[3] != 0)
            .ok_or(Refusal::Ethernet)?;
        let max_segment = word(ethernet, 8);
        if usize::from(max_segment) < MAX_FRAME {
            return Err(Refusal::Segment(max_segment));
        }
        let capabilities = match model {
            Model::Ecm => 0,
            Model::Ncm => config
                .setting(control, 0)
                .find_map(|item| functional(item, NCM, NCM_LEN))
                .ok_or(Refusal::Ncm)?[5],
        };
        let notify = config
            .endpoints(control, 0)
            .find(|e| e.is_in() && e.transfer_type() == TransferType::Interrupt);
        Ok(Self {
            model,
            control,
            data,
            alternate,
            bulk_in,
            bulk_out,
            notify,
            mac_string: ethernet[3],
            max_segment,
            capabilities,
        })
    }
}

/// The address an iMACAddress string descriptor (the whole descriptor,
/// bLength/bDescriptorType included) names; a group (multicast) or all-zero
/// address is refused. The string is twelve hexadecimal digits, most
/// significant first (ECM 1.2 §5.4).
pub fn mac_address(descriptor: &[u8]) -> Option<[u8; 6]> {
    let len = usize::from(*descriptor.first()?);
    if len != 2 + 24 || descriptor.len() < len || descriptor[1] != kind::STRING {
        return None;
    }
    let mut mac = [0u8; 6];
    for (at, &[low, high]) in descriptor[2..len].as_chunks::<2>().0.iter().enumerate() {
        if high != 0 {
            return None;
        }
        let digit = char::from(low).to_digit(16)? as u8;
        mac[at / 2] |= digit << if at % 2 == 0 { 4 } else { 0 };
    }
    (mac[0] & 1 == 0 && mac != [0; 6]).then_some(mac)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notification {
    Connection(bool),
    Other,
}

/// A notification from the control interface's interrupt endpoint. `None`
/// for anything else: a bad request type, a NETWORK_CONNECTION whose wValue
/// is neither 0 nor 1 or whose wLength is not 0, or a report shorter than 8
/// bytes. `wIndex` is not checked: QEMU names the data interface there.
pub fn notification(report: &[u8]) -> Option<Notification> {
    if report.len() < NOTIFICATION_LEN
        || report[0] != DIRECTION_IN | TYPE_CLASS | RECIPIENT_INTERFACE
    {
        return None;
    }
    if report[1] != NETWORK_CONNECTION {
        return Some(Notification::Other);
    }
    match (word(report, 2), word(report, 6)) {
        (value @ (0 | 1), 0) => Some(Notification::Connection(value == 1)),
        _ => None,
    }
}

impl Setup {
    pub fn set_ethernet_packet_filter(interface: u8, filter: u16) -> Self {
        Self::cdc_out(interface, SET_ETHERNET_PACKET_FILTER, filter, 0)
    }

    pub fn get_ntb_parameters(interface: u8) -> Self {
        Self {
            request_type: DIRECTION_IN | TYPE_CLASS | RECIPIENT_INTERFACE,
            request: GET_NTB_PARAMETERS,
            value: 0,
            index: interface.into(),
            length: ntb::Parameters::LEN as u16,
        }
    }

    pub fn set_ntb_format_16(interface: u8) -> Self {
        Self::cdc_out(interface, SET_NTB_FORMAT, 0, 0)
    }

    /// The data stage is `dwNtbInMaxSize`, then, for a function whose
    /// capabilities carry [`capability::NTB_INPUT_SIZE_8`], a datagram limit
    /// and a reserved word: [`ntb_input_size`]'s bytes.
    pub fn set_ntb_input_size(interface: u8, capabilities: u8) -> Self {
        let length = if capabilities & capability::NTB_INPUT_SIZE_8 != 0 {
            8
        } else {
            4
        };
        Self::cdc_out(interface, SET_NTB_INPUT_SIZE, 0, length)
    }

    pub fn set_crc_mode_off(interface: u8) -> Self {
        Self::cdc_out(interface, SET_CRC_MODE, 0, 0)
    }

    fn cdc_out(interface: u8, request: u8, value: u16, length: u16) -> Self {
        Self {
            request_type: TYPE_CLASS | RECIPIENT_INTERFACE,
            request,
            value,
            index: interface.into(),
            length,
        }
    }
}

/// `SET_NTB_INPUT_SIZE`'s data, of which the request's length is sent: no
/// datagram limit in the long form.
pub fn ntb_input_size(size: u32) -> [u8; 8] {
    let [a, b, c, d] = size.to_le_bytes();
    [a, b, c, d, 0, 0, 0, 0]
}

/// Whether a bulk OUT transfer of `length` bytes is followed by a zero-length
/// packet: when it is a whole number of `max_packet` packets (and not
/// empty), and shorter than `limit` (NCM: an NTB of exactly
/// dwNtbOutMaxSize needs none; pass usize::MAX for ECM).
pub fn needs_zlp(length: usize, max_packet: u16, limit: usize) -> bool {
    length != 0 && length.is_multiple_of(usize::from(max_packet)) && length < limit
}

#[cfg(test)]
mod tests;
