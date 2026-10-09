//! Descriptors (USB 2.0 §9.5–9.6, USB 3.2 §9.6.7, the Interface Association
//! ECN). A parser reads only within its bytes and each descriptor's `bLength`.

use super::Speed;

pub mod kind {
    pub const DEVICE: u8 = 1;
    pub const CONFIGURATION: u8 = 2;
    pub const STRING: u8 = 3;
    pub const INTERFACE: u8 = 4;
    pub const ENDPOINT: u8 = 5;
    pub const INTERFACE_ASSOCIATION: u8 = 11;
    pub const SUPERSPEED_COMPANION: u8 = 48;
}

pub mod class {
    pub const PER_INTERFACE: u8 = 0x00;
    pub const COMMUNICATIONS: u8 = 0x02;
    pub const HUB: u8 = 0x09;
    pub const MISCELLANEOUS: u8 = 0xef;
    pub const VENDOR: u8 = 0xff;
}

const CS_INTERFACE: u8 = 0x24;
const UNION: u8 = 0x06;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    Short,
    Kind,
    /// `bLength` is under the fixed part.
    Length,
    TotalLength,
    /// A descriptor's `bLength` is under 2 or runs past `wTotalLength`.
    Chain,
    /// `bConfigurationValue` 0, which `SET_CONFIGURATION` takes as unconfigure.
    Value,
}

fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn fixed(bytes: &[u8], kind: u8, len: usize) -> Result<&[u8], Malformed> {
    if bytes.len() < len {
        return Err(Malformed::Short);
    }
    if bytes[1] != kind {
        return Err(Malformed::Kind);
    }
    if usize::from(bytes[0]) < len {
        return Err(Malformed::Length);
    }
    Ok(&bytes[..len])
}

pub const DEVICE_LEN: usize = 18;
/// What a full-speed device's EP0 packet size is read from.
pub const DEVICE_PREFIX_LEN: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub bcd_usb: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub max_packet0: u8,
    pub vendor: u16,
    pub product: u16,
    pub bcd_device: u16,
    pub manufacturer_string: u8,
    pub product_string: u8,
    pub serial_string: u8,
    pub configurations: u8,
}

impl DeviceDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self, Malformed> {
        let b = fixed(bytes, kind::DEVICE, DEVICE_LEN)?;
        Ok(Self {
            bcd_usb: word(b, 2),
            class: b[4],
            subclass: b[5],
            protocol: b[6],
            max_packet0: b[7],
            vendor: word(b, 8),
            product: word(b, 10),
            bcd_device: word(b, 12),
            manufacturer_string: b[14],
            product_string: b[15],
            serial_string: b[16],
            configurations: b[17],
        })
    }

    pub fn max_packet0_of(prefix: &[u8]) -> Result<u8, Malformed> {
        let b = fixed(prefix, kind::DEVICE, DEVICE_PREFIX_LEN)?;
        Ok(b[7])
    }

    pub fn is_hub(&self) -> bool {
        self.class == class::HUB
    }
}

/// EP0's packet size from `bMaxPacketSize0` at `speed`, if that speed
/// allows the value (USB 2.0 §5.5.3, USB 3.2 §9.6.1).
pub fn ep0_max_packet(speed: Speed, raw: u8) -> Option<u16> {
    match (speed, raw) {
        (Speed::Low, 8) => Some(8),
        (Speed::Full, 8 | 16 | 32 | 64) => Some(raw.into()),
        (Speed::High, 64) => Some(64),
        (Speed::Super | Speed::SuperPlus, 9) => Some(512),
        _ => None,
    }
}

pub const CONFIGURATION_LEN: usize = 9;
const SELF_POWERED: u8 = 1 << 6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConfigurationHeader {
    pub total_length: u16,
    pub interfaces: u8,
    pub value: u8,
    pub string: u8,
    pub attributes: u8,
    pub max_power: u8,
}

impl ConfigurationHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self, Malformed> {
        let b = fixed(bytes, kind::CONFIGURATION, CONFIGURATION_LEN)?;
        let header = Self {
            total_length: word(b, 2),
            interfaces: b[4],
            value: b[5],
            string: b[6],
            attributes: b[7],
            max_power: b[8],
        };
        if usize::from(header.total_length) < CONFIGURATION_LEN {
            return Err(Malformed::TotalLength);
        }
        if header.value == 0 {
            return Err(Malformed::Value);
        }
        Ok(header)
    }

    pub fn self_powered(&self) -> bool {
        self.attributes & SELF_POWERED != 0
    }

    /// `bMaxPower` in milliamps: units of 2 mA below SuperSpeed and of 8 mA
    /// at it.
    pub fn max_power_ma(&self, speed: Speed) -> u32 {
        u32::from(self.max_power) * if speed.is_super() { 8 } else { 2 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interface {
    pub number: u8,
    pub alternate: u8,
    pub endpoints: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub string: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Companion {
    pub max_burst: u8,
    pub attributes: u8,
    pub bytes_per_interval: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Endpoint {
    pub address: u8,
    pub attributes: u8,
    /// `wMaxPacketSize` as sent: the size in bits 10:0, a high-speed
    /// periodic endpoint's extra transactions in 12:11.
    pub max_packet: u16,
    pub interval: u8,
    pub companion: Option<Companion>,
}

impl Endpoint {
    pub fn number(&self) -> u8 {
        self.address & 0x0f
    }

    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    pub fn transfer_type(&self) -> TransferType {
        match self.attributes & 0b11 {
            0 => TransferType::Control,
            1 => TransferType::Isochronous,
            2 => TransferType::Bulk,
            _ => TransferType::Interrupt,
        }
    }

    pub fn max_packet_size(&self) -> u16 {
        self.max_packet & 0x7ff
    }

    /// A high-speed periodic endpoint's transactions per microframe past one.
    pub fn extra_transactions(&self) -> u8 {
        ((self.max_packet >> 11) & 0b11) as u8
    }

    /// Endpoint 0 is not described, and an endpoint that moves nothing
    /// cannot be given a ring.
    pub fn is_usable(&self) -> bool {
        self.number() != 0
            && self.max_packet_size() != 0
            && self.transfer_type() != TransferType::Control
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Association {
    pub first_interface: u8,
    pub interfaces: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub string: u8,
}

impl Association {
    fn covers(&self, interface: u8) -> bool {
        interface >= self.first_interface
            && u16::from(interface) < u16::from(self.first_interface) + u16::from(self.interfaces)
    }
}

/// A descriptor of a configuration; one too short for its type is `Other`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item<'a> {
    Interface(Interface),
    Endpoint(Endpoint),
    Association(Association),
    Other { kind: u8, bytes: &'a [u8] },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Configuration<'a> {
    pub header: ConfigurationHeader,
    bytes: &'a [u8],
}

/// `(bDescriptorType, bytes)` of each descriptor after the configuration's.
#[derive(Clone, Copy)]
pub struct Raw<'a> {
    bytes: &'a [u8],
}

impl<'a> Iterator for Raw<'a> {
    type Item = (u8, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let len = usize::from(*self.bytes.first()?);
        if len < 2 || len > self.bytes.len() {
            self.bytes = &[];
            return None;
        }
        let (this, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Some((this[1], this))
    }
}

fn decode(kind: u8, b: &[u8]) -> Item<'_> {
    match kind {
        kind::INTERFACE if b.len() >= 9 => Item::Interface(Interface {
            number: b[2],
            alternate: b[3],
            endpoints: b[4],
            class: b[5],
            subclass: b[6],
            protocol: b[7],
            string: b[8],
        }),
        kind::ENDPOINT if b.len() >= 7 => Item::Endpoint(Endpoint {
            address: b[2],
            attributes: b[3],
            max_packet: word(b, 4),
            interval: b[6],
            companion: None,
        }),
        kind::INTERFACE_ASSOCIATION if b.len() >= 8 => Item::Association(Association {
            first_interface: b[2],
            interfaces: b[3],
            class: b[4],
            subclass: b[5],
            protocol: b[6],
            string: b[7],
        }),
        _ => Item::Other { kind, bytes: b },
    }
}

fn companion(kind: u8, b: &[u8]) -> Option<Companion> {
    (kind == kind::SUPERSPEED_COMPANION && b.len() >= 6).then(|| Companion {
        max_burst: b[2],
        attributes: b[3],
        bytes_per_interval: word(b, 4),
    })
}

/// An endpoint carries the SuperSpeed companion that follows it.
#[derive(Clone, Copy)]
pub struct Items<'a> {
    raw: Raw<'a>,
}

impl<'a> Iterator for Items<'a> {
    type Item = Item<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let (kind, bytes) = self.raw.next()?;
        let mut item = decode(kind, bytes);
        if let Item::Endpoint(endpoint) = &mut item {
            let mut ahead = self.raw;
            if let Some(found) = ahead.next().and_then(|(k, b)| companion(k, b)) {
                endpoint.companion = Some(found);
                self.raw = ahead;
            }
        }
        Some(item)
    }
}

/// Functions past this are left unbound.
pub const MAX_FUNCTIONS: usize = 16;

/// What a driver binds: one interface, or the interfaces an association or a
/// communications class union groups.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Function {
    pub first_interface: u8,
    pub interfaces: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Functions {
    list: [Function; MAX_FUNCTIONS],
    len: usize,
    pub dropped: usize,
}

impl Functions {
    pub fn as_slice(&self) -> &[Function] {
        &self.list[..self.len]
    }

    fn push(&mut self, function: Function) {
        if self.len == MAX_FUNCTIONS {
            self.dropped += 1;
        } else {
            self.list[self.len] = function;
            self.len += 1;
        }
    }

    fn holds(&self, interface: u8) -> bool {
        self.as_slice().iter().any(|f| {
            interface >= f.first_interface
                && u16::from(interface) < u16::from(f.first_interface) + u16::from(f.interfaces)
        })
    }
}

impl<'a> Configuration<'a> {
    /// Bytes past `wTotalLength` are ignored.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Malformed> {
        let header = ConfigurationHeader::parse(bytes)?;
        let total = usize::from(header.total_length);
        if bytes.len() < total {
            return Err(Malformed::TotalLength);
        }
        let bytes = &bytes[..total];
        let mut at = usize::from(bytes[0]);
        if at > total {
            return Err(Malformed::Chain);
        }
        while at < total {
            let len = usize::from(bytes[at]);
            if len < 2 || at + len > total {
                return Err(Malformed::Chain);
            }
            at += len;
        }
        Ok(Self { header, bytes })
    }

    pub fn raw(&self) -> Raw<'a> {
        Raw {
            bytes: &self.bytes[usize::from(self.bytes[0])..],
        }
    }

    pub fn items(&self) -> Items<'a> {
        Items { raw: self.raw() }
    }

    pub fn interfaces(&self) -> impl Iterator<Item = Interface> + 'a {
        self.items().filter_map(|item| match item {
            Item::Interface(interface) => Some(interface),
            _ => None,
        })
    }

    /// The items after an interface's descriptor, up to the next interface or
    /// association.
    pub fn setting(&self, number: u8, alternate: u8) -> impl Iterator<Item = Item<'a>> + 'a {
        let mut inside = false;
        self.items().filter(move |item| {
            match item {
                Item::Interface(i) => {
                    inside = i.number == number && i.alternate == alternate;
                    return false;
                }
                Item::Association(_) => inside = false,
                _ => {}
            }
            inside
        })
    }

    pub fn endpoints(&self, number: u8, alternate: u8) -> impl Iterator<Item = Endpoint> + 'a {
        self.setting(number, alternate)
            .filter_map(|item| match item {
                Item::Endpoint(e) if e.is_usable() => Some(e),
                _ => None,
            })
    }

    /// Alternate setting 0 of every interface. An association's interfaces
    /// are one function named by its class triple, and so is a communications
    /// interface with the interfaces its Union functional descriptor (CDC 1.2
    /// §5.2.3.2) names when they are the next numbers and in no other
    /// function, named by the communications interface's triple.
    pub fn functions(&self) -> Functions {
        let mut associations = [Association::default(); MAX_FUNCTIONS];
        let mut count = 0;
        for item in self.items() {
            if let Item::Association(a) = item
                && a.interfaces != 0
                && count < MAX_FUNCTIONS
            {
                associations[count] = a;
                count += 1;
            }
        }
        let associations = &associations[..count];
        let mut functions = Functions::default();
        for interface in self.interfaces().filter(|i| i.alternate == 0) {
            if functions.holds(interface.number) {
                continue;
            }
            let function = match associations.iter().find(|a| a.covers(interface.number)) {
                Some(a) => Function {
                    first_interface: a.first_interface,
                    interfaces: a.interfaces,
                    class: a.class,
                    subclass: a.subclass,
                    protocol: a.protocol,
                },
                None => Function {
                    first_interface: interface.number,
                    interfaces: 1 + self.union(&interface, associations, &functions),
                    class: interface.class,
                    subclass: interface.subclass,
                    protocol: interface.protocol,
                },
            };
            functions.push(function);
        }
        functions
    }

    /// How many interfaces a communications interface's union adds to its
    /// function: 0 unless they follow it and belong to nothing else.
    fn union(&self, control: &Interface, associations: &[Association], taken: &Functions) -> u8 {
        if control.class != class::COMMUNICATIONS {
            return 0;
        }
        let Some(subordinates) = self.setting(control.number, 0).find_map(|item| match item {
            Item::Other {
                kind: CS_INTERFACE,
                bytes,
            } if bytes.len() >= 4 && bytes[2] == UNION => Some(bytes),
            _ => None,
        }) else {
            return 0;
        };
        if subordinates[3] != control.number || subordinates.len() < 5 {
            return 0;
        }
        let subordinates = &subordinates[4..];
        let consecutive = subordinates.iter().enumerate().all(|(k, &number)| {
            usize::from(number) == usize::from(control.number) + 1 + k
                && !taken.holds(number)
                && !associations.iter().any(|a| a.covers(number))
                && self
                    .interfaces()
                    .any(|i| i.number == number && i.alternate == 0)
        });
        if consecutive {
            subordinates.len() as u8
        } else {
            0
        }
    }

    pub fn has_interface_class(&self, class: u8) -> bool {
        self.interfaces()
            .any(|i| i.alternate == 0 && i.class == class)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::vec::Vec;

    pub fn device(class: u8, max_packet0: u8, vendor: u16, product: u16, configs: u8) -> [u8; 18] {
        let [v0, v1] = vendor.to_le_bytes();
        let [p0, p1] = product.to_le_bytes();
        [
            18,
            1,
            0x00,
            0x02,
            class,
            0,
            0,
            max_packet0,
            v0,
            v1,
            p0,
            p1,
            0x00,
            0x01,
            1,
            2,
            3,
            configs,
        ]
    }

    pub fn configuration(value: u8, max_power: u8, body: &[&[u8]]) -> Vec<u8> {
        let mut bytes = Vec::from([9, 2, 0, 0, 0, value, 0, 0xc0, max_power]);
        let mut interfaces = std::collections::BTreeSet::new();
        for d in body {
            if d[1] == kind::INTERFACE {
                interfaces.insert(d[2]);
            }
            bytes.extend_from_slice(d);
        }
        let total = (bytes.len() as u16).to_le_bytes();
        bytes[2..4].copy_from_slice(&total);
        bytes[4] = interfaces.len() as u8;
        bytes
    }

    pub fn interface(number: u8, alternate: u8, endpoints: u8, class: [u8; 3]) -> [u8; 9] {
        [
            9, 4, number, alternate, endpoints, class[0], class[1], class[2], 0,
        ]
    }

    pub fn endpoint(address: u8, attributes: u8, max_packet: u16, interval: u8) -> [u8; 7] {
        let [m0, m1] = max_packet.to_le_bytes();
        [7, 5, address, attributes, m0, m1, interval]
    }

    pub fn association(first: u8, count: u8, class: [u8; 3]) -> [u8; 8] {
        [8, 11, first, count, class[0], class[1], class[2], 0]
    }

    pub const ECM_ETHERNET: [u8; 13] = [13, 0x24, 0x0f, 3, 0, 0, 0, 0, 0xea, 0x05, 0, 0, 0];

    /// QEMU usb-net's ECM configuration.
    pub fn qemu_ecm() -> Vec<u8> {
        configuration(
            1,
            50,
            &[
                &interface(0, 0, 1, [2, 6, 0]),
                &[5, 0x24, 0x00, 0x10, 0x01],
                &[5, 0x24, 0x06, 0, 1],
                &ECM_ETHERNET,
                &endpoint(0x81, 3, 16, 32),
                &interface(1, 0, 0, [10, 0, 0]),
                &interface(1, 1, 2, [10, 0, 0]),
                &endpoint(0x82, 2, 64, 0),
                &endpoint(0x02, 2, 64, 0),
            ],
        )
    }

    /// QEMU usb-net's RNDIS configuration.
    pub fn qemu_rndis() -> Vec<u8> {
        configuration(
            2,
            50,
            &[
                &interface(0, 0, 1, [2, 2, 0xff]),
                &[5, 0x24, 0x00, 0x10, 0x01],
                &[5, 0x24, 0x01, 0x00, 0x01],
                &[4, 0x24, 0x02, 0x00],
                &[5, 0x24, 0x06, 0, 1],
                &endpoint(0x81, 3, 8, 32),
                &interface(1, 0, 2, [10, 0, 0]),
                &endpoint(0x82, 2, 64, 0),
                &endpoint(0x02, 2, 64, 0),
            ],
        )
    }

    #[test]
    fn a_device_descriptor_decodes_and_its_prefix_carries_ep0() {
        let bytes = device(0, 64, 0x046d, 0xc52b, 1);
        let d = DeviceDescriptor::parse(&bytes).unwrap();
        assert_eq!((d.vendor, d.product, d.max_packet0), (0x046d, 0xc52b, 64));
        assert_eq!(
            (d.bcd_usb, d.bcd_device, d.configurations),
            (0x0200, 0x0100, 1)
        );
        assert_eq!(d.product_string, 2);
        assert_eq!(DeviceDescriptor::max_packet0_of(&bytes[..8]), Ok(64));
        assert_eq!(
            DeviceDescriptor::max_packet0_of(&bytes[..7]),
            Err(Malformed::Short)
        );
        assert_eq!(DeviceDescriptor::parse(&bytes[..17]), Err(Malformed::Short));
        let mut wrong = bytes;
        wrong[1] = 2;
        assert_eq!(DeviceDescriptor::parse(&wrong), Err(Malformed::Kind));
        wrong = bytes;
        wrong[0] = 12;
        assert_eq!(DeviceDescriptor::parse(&wrong), Err(Malformed::Length));
    }

    #[test]
    fn ep0_sizes_are_held_to_the_speed() {
        assert_eq!(ep0_max_packet(Speed::Full, 32), Some(32));
        assert_eq!(ep0_max_packet(Speed::Full, 9), None);
        assert_eq!(ep0_max_packet(Speed::Low, 64), None);
        assert_eq!(ep0_max_packet(Speed::High, 64), Some(64));
        assert_eq!(ep0_max_packet(Speed::High, 8), None);
        assert_eq!(ep0_max_packet(Speed::Super, 9), Some(512));
        assert_eq!(ep0_max_packet(Speed::Super, 64), None);
    }

    #[test]
    fn a_storage_configuration_walks_to_its_endpoints() {
        let bytes = configuration(
            1,
            50,
            &[
                &interface(0, 0, 2, [8, 6, 0x50]),
                &endpoint(0x81, 2, 1024, 0),
                &[6, 48, 15, 0, 0, 0],
                &endpoint(0x02, 2, 1024, 0),
                &[6, 48, 15, 0, 0, 0],
            ],
        );
        let config = Configuration::parse(&bytes).unwrap();
        assert_eq!(config.header.value, 1);
        assert_eq!(config.header.max_power_ma(Speed::Super), 400);
        assert_eq!(config.header.max_power_ma(Speed::High), 100);
        assert!(config.header.self_powered());
        let endpoints: Vec<_> = config.endpoints(0, 0).collect();
        assert_eq!(endpoints.len(), 2);
        assert!(endpoints[0].is_in() && endpoints[0].number() == 1);
        assert_eq!(endpoints[0].transfer_type(), TransferType::Bulk);
        assert_eq!(endpoints[1].companion.unwrap().max_burst, 15);
        let functions = config.functions();
        assert_eq!(
            functions.as_slice(),
            &[Function {
                first_interface: 0,
                interfaces: 1,
                class: 8,
                subclass: 6,
                protocol: 0x50,
            }]
        );
    }

    #[test]
    fn an_association_groups_its_interfaces_into_one_function() {
        let cs_interface = [5, 0x24, 0, 0x10, 0x01];
        let bytes = configuration(
            1,
            0,
            &[
                &association(0, 2, [2, 6, 0]),
                &interface(0, 0, 1, [2, 6, 0]),
                &cs_interface,
                &endpoint(0x83, 3, 16, 9),
                &interface(1, 0, 0, [10, 0, 0]),
                &interface(1, 1, 2, [10, 0, 0]),
                &endpoint(0x81, 2, 512, 0),
                &endpoint(0x02, 2, 512, 0),
                &interface(2, 0, 1, [3, 1, 1]),
                &endpoint(0x84, 3, 8, 10),
            ],
        );
        let config = Configuration::parse(&bytes).unwrap();
        let functions = config.functions();
        assert_eq!(functions.as_slice().len(), 2);
        assert_eq!(functions.as_slice()[0].interfaces, 2);
        assert_eq!(functions.as_slice()[0].class, 2);
        assert_eq!(functions.as_slice()[1].first_interface, 2);
        assert_eq!(functions.as_slice()[1].class, 3);
        assert_eq!(config.endpoints(1, 0).count(), 0);
        assert_eq!(config.endpoints(1, 1).count(), 2);
        let class: Vec<_> = config
            .setting(0, 0)
            .filter_map(|item| match item {
                Item::Other { kind, bytes } => Some((kind, bytes.len())),
                _ => None,
            })
            .collect();
        assert_eq!(class, [(0x24, 5)]);
    }

    #[test]
    fn misstated_lengths_are_refused_whole() {
        let mut bytes = configuration(1, 0, &[&interface(0, 0, 1, [3, 1, 1])]);
        bytes.extend_from_slice(&endpoint(0x81, 3, 8, 10));
        assert_eq!(
            Configuration::parse(&bytes)
                .unwrap()
                .endpoints(0, 0)
                .count(),
            0,
            "bytes past wTotalLength are not the configuration's"
        );
        let mut long = bytes.clone();
        long[2] += 20;
        assert_eq!(Configuration::parse(&long), Err(Malformed::TotalLength));
        let mut chain = configuration(1, 0, &[&interface(0, 0, 0, [3, 1, 1])]);
        chain[9] = 1;
        assert_eq!(Configuration::parse(&chain), Err(Malformed::Chain));
        chain[9] = 30;
        assert_eq!(Configuration::parse(&chain), Err(Malformed::Chain));
        let mut tiny = configuration(1, 0, &[]);
        tiny[2] = 4;
        assert_eq!(Configuration::parse(&tiny), Err(Malformed::TotalLength));
    }

    #[test]
    fn unusable_endpoints_and_short_descriptors_are_skipped() {
        let short_endpoint = [6, 5, 0x82, 2, 0, 2];
        let bytes = configuration(
            1,
            0,
            &[
                &interface(0, 0, 4, [0xff, 0, 0]),
                &endpoint(0x80, 2, 512, 0),
                &endpoint(0x81, 2, 0, 0),
                &short_endpoint,
                &endpoint(0x03, 0, 64, 0),
                &endpoint(0x84, 3, 64, 1),
            ],
        );
        let config = Configuration::parse(&bytes).unwrap();
        let endpoints: Vec<_> = config.endpoints(0, 0).map(|e| e.address).collect();
        assert_eq!(endpoints, [0x84]);
    }

    #[test]
    fn functions_past_the_table_are_counted_not_kept() {
        let interfaces: Vec<[u8; 9]> = (0..20).map(|n| interface(n, 0, 0, [0xff, 0, 0])).collect();
        let body: Vec<&[u8]> = interfaces.iter().map(|i| &i[..]).collect();
        let bytes = configuration(1, 0, &body);
        let functions = Configuration::parse(&bytes).unwrap().functions();
        assert_eq!(functions.as_slice().len(), MAX_FUNCTIONS);
        assert_eq!(functions.dropped, 4);
    }

    #[test]
    fn a_communications_union_groups_the_interfaces_after_it() {
        let one = |bytes: &[u8]| {
            let functions = Configuration::parse(bytes).unwrap().functions();
            assert_eq!(functions.as_slice().len(), 1);
            functions.as_slice()[0]
        };
        assert_eq!(
            one(&qemu_ecm()),
            Function {
                first_interface: 0,
                interfaces: 2,
                class: 2,
                subclass: 6,
                protocol: 0,
            }
        );
        assert_eq!(
            one(&qemu_rndis()),
            Function {
                first_interface: 0,
                interfaces: 2,
                class: 2,
                subclass: 2,
                protocol: 0xff,
            }
        );
        let three = configuration(
            1,
            0,
            &[
                &interface(4, 0, 0, [2, 0x0d, 0]),
                &[6, 0x24, 0x06, 4, 5, 6],
                &interface(5, 0, 0, [10, 0, 1]),
                &interface(6, 0, 0, [10, 0, 1]),
            ],
        );
        assert_eq!(one(&three).interfaces, 3);
    }

    #[test]
    fn a_union_naming_anything_else_groups_nothing() {
        let functions = |union: &[u8], data: u8| -> Vec<(u8, u8)> {
            let bytes = configuration(
                1,
                0,
                &[
                    &interface(0, 0, 0, [2, 6, 0]),
                    union,
                    &interface(data, 0, 0, [10, 0, 0]),
                ],
            );
            Configuration::parse(&bytes)
                .unwrap()
                .functions()
                .as_slice()
                .iter()
                .map(|f| (f.first_interface, f.interfaces))
                .collect()
        };
        assert_eq!(functions(&[5, 0x24, 6, 0, 1], 1), [(0, 2)]);
        assert_eq!(functions(&[5, 0x24, 6, 0, 2], 2), [(0, 1), (2, 1)]);
        assert_eq!(functions(&[5, 0x24, 6, 0, 1], 2), [(0, 1), (2, 1)]);
        assert_eq!(functions(&[5, 0x24, 6, 1, 1], 1), [(0, 1), (1, 1)]);
        assert_eq!(functions(&[4, 0x24, 6, 0], 1), [(0, 1), (1, 1)]);
        assert_eq!(functions(&[6, 0x24, 6, 0, 1, 2], 1), [(0, 1), (1, 1)]);
        let associated = configuration(
            1,
            0,
            &[
                &interface(0, 0, 0, [2, 6, 0]),
                &[5, 0x24, 6, 0, 1],
                &association(1, 1, [10, 0, 0]),
                &interface(1, 0, 0, [10, 0, 0]),
            ],
        );
        let functions = Configuration::parse(&associated).unwrap().functions();
        assert_eq!(functions.as_slice().len(), 2);
    }

    #[test]
    fn mutated_descriptors_never_panic_or_read_past_their_bytes() {
        let associated = configuration(
            2,
            250,
            &[
                &association(0, 2, [2, 6, 0]),
                &interface(0, 0, 1, [2, 6, 0]),
                &[5, 0x24, 6, 0, 1],
                &endpoint(0x83, 3, 16, 9),
                &interface(1, 0, 0, [10, 0, 0]),
                &interface(1, 1, 2, [10, 0, 0]),
                &endpoint(0x81, 2, 1024, 0),
                &[6, 48, 15, 0, 0, 0],
                &endpoint(0x02, 2, 1024, 0),
            ],
        );
        let exercise = |b: &[u8]| {
            let _ = DeviceDescriptor::parse(b);
            let _ = DeviceDescriptor::max_packet0_of(b);
            if let Ok(config) = Configuration::parse(b) {
                let _ = config.functions();
                for interface in config.interfaces() {
                    let _ = config
                        .endpoints(interface.number, interface.alternate)
                        .count();
                    let _ = config
                        .setting(interface.number, interface.alternate)
                        .count();
                }
                let _ = config.raw().count();
            }
        };
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for bytes in [associated, qemu_ecm(), qemu_rndis()] {
            for len in 0..=bytes.len() {
                exercise(&bytes[..len]);
            }
            for at in 0..bytes.len() {
                for _ in 0..16 {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    let mut mutated = bytes.clone();
                    mutated[at] = state as u8;
                    exercise(&mutated);
                    let cut = (state >> 8) as usize % (bytes.len() + 1);
                    exercise(&mutated[..cut]);
                }
            }
        }
    }
}
