//! Simulated devices answering control requests from their descriptors, with
//! the faults a test asks for.

use crate::device::Speed;
use crate::device::descriptor::kind;
use crate::device::descriptor::tests::{configuration, device, endpoint, interface};
use crate::device::request::Setup;
use std::boxed::Box;
use std::vec;
use std::vec::Vec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Data(Vec<u8>),
    Stall,
    /// More than the request asked for.
    Babble,
    /// No handshake: a transaction error.
    Silent,
}

#[derive(Clone, Debug, Default)]
pub struct Faults {
    /// `GET_DESCRIPTOR` of this type stalls the first `stalls` times, or
    /// always when `stalls` is 0.
    pub stall_descriptor: Option<u8>,
    pub stalls: u32,
    pub babble_descriptor: Option<u8>,
    /// `GET_DESCRIPTOR` of a type sends only this many bytes.
    pub truncate: Option<(u8, usize)>,
    /// The device leaves during its `n`th control request.
    pub pull_at: Option<u32>,
    pub silent: bool,
    /// Address Device fails this many times.
    pub refuse_address: u32,
    pub stall_port_clear: bool,
    /// DCI bits of endpoints whose `CLEAR_FEATURE(ENDPOINT_HALT)` stalls.
    pub stall_clear_halt: u32,
}

#[derive(Clone, Debug)]
pub struct SimHubPort {
    pub device: Option<Box<SimDevice>>,
    pub powered: bool,
    pub enabled: bool,
    pub resetting: bool,
    /// `wPortChange` bits.
    pub changes: u16,
}

#[derive(Clone, Debug)]
pub struct SimHub {
    pub super_speed: bool,
    pub ports: Vec<SimHubPort>,
    pub think_time: u8,
    /// In 2 ms units.
    pub power_good: u8,
    pub depth: Option<u8>,
    /// Ports that power on only when told to.
    pub power_switching: bool,
    /// `CLEAR_TT_BUFFER`'s `wValue` and `wIndex`, as each arrived.
    pub tt_clears: Vec<(u16, u16)>,
}

#[derive(Clone, Debug)]
pub struct SimDevice {
    pub speed: Speed,
    pub descriptor: Vec<u8>,
    pub configurations: Vec<Vec<u8>>,
    pub hub: Option<SimHub>,
    pub self_powered: bool,
    pub faults: Faults,
    pub address: u8,
    pub configuration: u8,
    pub slot: u8,
    pub requests: Vec<Setup>,
    /// Set by [`Faults::pull_at`].
    pub pull_me: bool,
    /// Reset, not yet addressed.
    pub default_state: bool,
    /// DCI bits of endpoints that STALL until `CLEAR_FEATURE(ENDPOINT_HALT)`.
    pub halted_endpoints: u32,
    /// DCI bits of the device's data toggles: the next packet's DATA1.
    pub toggles: u32,
    pub storage: Option<Box<super::storage::SimStorage>>,
}

fn max_packet0(speed: Speed) -> u8 {
    match speed {
        Speed::Low | Speed::Full => 8,
        Speed::High => 64,
        _ => 9,
    }
}

fn bcd(speed: Speed) -> u16 {
    if speed.is_super() { 0x0300 } else { 0x0200 }
}

fn device_bytes(speed: Speed, class: u8, protocol: u8, vendor: u16, product: u16) -> Vec<u8> {
    let mut bytes = device(class, max_packet0(speed), vendor, product, 1).to_vec();
    bytes[2..4].copy_from_slice(&bcd(speed).to_le_bytes());
    bytes[6] = protocol;
    bytes
}

impl SimDevice {
    fn new(speed: Speed, descriptor: Vec<u8>, configurations: Vec<Vec<u8>>) -> Self {
        Self {
            speed,
            descriptor,
            configurations,
            hub: None,
            self_powered: true,
            faults: Faults::default(),
            address: 0,
            configuration: 0,
            slot: 0,
            requests: Vec::new(),
            pull_me: false,
            default_state: false,
            halted_endpoints: 0,
            toggles: 0,
            storage: None,
        }
    }

    /// A Bulk-Only stick, as QEMU's `usb-storage` describes itself.
    pub fn storage(speed: Speed) -> Self {
        let max_packet = match speed {
            Speed::Low | Speed::Full => 64,
            Speed::High => 512,
            _ => 1024,
        };
        let companion: &[u8] = &[6, kind::SUPERSPEED_COMPANION, 15, 0, 0, 0];
        let mut body: Vec<Vec<u8>> = vec![interface(0, 0, 2, [8, 6, 0x50]).to_vec()];
        for address in [0x81, 0x02] {
            body.push(endpoint(address, 2, max_packet, 0).to_vec());
            if speed.is_super() {
                body.push(companion.to_vec());
            }
        }
        let refs: Vec<&[u8]> = body.iter().map(Vec::as_slice).collect();
        Self::new(
            speed,
            device_bytes(speed, 0, 0, 0x46f4, 0x0001),
            vec![configuration(1, 50, &refs)],
        )
    }

    /// A bus-powered boot keyboard.
    pub fn keyboard(speed: Speed) -> Self {
        let hid: &[u8] = &[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0];
        let config = configuration(
            1,
            50,
            &[
                &interface(0, 0, 1, [3, 1, 1]),
                hid,
                &endpoint(0x81, 3, 8, 10),
            ],
        );
        let mut keyboard = Self::new(
            speed,
            device_bytes(speed, 0, 0, 0x0627, 0x0001),
            vec![config],
        );
        keyboard.self_powered = false;
        keyboard
    }

    /// A full-speed hub has no transaction translator.
    pub fn hub(speed: Speed, ports: u8) -> Self {
        let protocol = match speed {
            Speed::High => 1,
            s if s.is_super() => 3,
            _ => 0,
        };
        let report = (u16::from(ports) + 8) / 8;
        let companion: &[u8] = &[6, kind::SUPERSPEED_COMPANION, 0, 0, report as u8, 0];
        let interval = if speed >= Speed::High { 12 } else { 255 };
        let status = endpoint(0x81, 3, report, interval);
        let hub_interface = interface(0, 0, 1, [9, 0, 0]);
        let mut body: Vec<&[u8]> = vec![&hub_interface, &status];
        if speed.is_super() {
            body.push(companion);
        }
        let mut hub = Self::new(
            speed,
            device_bytes(speed, 9, protocol, 0x0409, 0x55aa),
            vec![configuration(1, 0, &body)],
        );
        hub.hub = Some(SimHub {
            super_speed: speed.is_super(),
            ports: (0..ports)
                .map(|_| SimHubPort {
                    device: None,
                    powered: false,
                    enabled: false,
                    resetting: false,
                    changes: 0,
                })
                .collect(),
            think_time: 1,
            power_good: 10,
            depth: None,
            power_switching: true,
            tt_clears: Vec::new(),
        });
        hub
    }

    pub fn bus_powered(mut self) -> Self {
        self.self_powered = false;
        if let Some(config) = self.configurations.first_mut() {
            config[7] = 0x80;
        }
        self
    }

    pub fn plug(&mut self, port: u8, device: SimDevice) {
        let hub = self.hub.as_mut().expect("a hub");
        let p = &mut hub.ports[usize::from(port) - 1];
        p.device = Some(Box::new(device));
        p.enabled = false;
        if p.powered {
            p.changes |= 1;
            if hub.super_speed {
                p.enabled = true;
            }
        }
    }

    pub fn unplug(&mut self, port: u8) -> Option<SimDevice> {
        let hub = self.hub.as_mut().expect("a hub");
        let p = &mut hub.ports[usize::from(port) - 1];
        let device = p.device.take()?;
        if p.powered {
            p.changes |= 1;
            if p.enabled && !hub.super_speed {
                p.changes |= 1 << 1;
            }
        }
        p.enabled = false;
        Some(*device)
    }

    pub fn child(&self, port: u8) -> Option<&SimDevice> {
        let hub = self.hub.as_ref()?;
        hub.ports
            .get(usize::from(port).checked_sub(1)?)?
            .device
            .as_deref()
    }

    pub fn child_mut(&mut self, port: u8) -> Option<&mut SimDevice> {
        let hub = self.hub.as_mut()?;
        hub.ports
            .get_mut(usize::from(port).checked_sub(1)?)?
            .device
            .as_deref_mut()
    }

    /// A packet routed through the port needs it enabled.
    pub fn port_enabled(&self, port: u8) -> bool {
        self.hub
            .as_ref()
            .and_then(|h| h.ports.get(usize::from(port).checked_sub(1)?))
            .is_some_and(|p| p.powered && p.enabled && p.device.is_some())
    }

    fn port_status(hub: &SimHub, port: usize) -> [u8; 4] {
        let p = &hub.ports[port];
        let connected = p.powered && p.device.is_some();
        let mut status = u16::from(connected) | u16::from(p.enabled && connected) << 1;
        status |= u16::from(p.resetting) << 4;
        if hub.super_speed {
            status |= u16::from(p.powered) << 9;
            if !connected {
                status |= 5 << 5;
            }
        } else {
            status |= u16::from(p.powered) << 8;
            match p.device.as_ref().map(|d| d.speed) {
                Some(Speed::Low) if connected => status |= 1 << 9,
                Some(Speed::High) if connected => status |= 1 << 10,
                _ => {}
            }
        }
        let [s0, s1] = status.to_le_bytes();
        let [c0, c1] = p.changes.to_le_bytes();
        [s0, s1, c0, c1]
    }

    /// `None` while nothing changed: the endpoint NAKs.
    pub fn status_report(&self) -> Option<Vec<u8>> {
        let hub = self.hub.as_ref()?;
        let mut report = vec![0u8; (hub.ports.len() + 8) / 8];
        let mut any = false;
        for (i, p) in hub.ports.iter().enumerate() {
            if p.changes != 0 {
                report[(i + 1) / 8] |= 1 << ((i + 1) % 8);
                any = true;
            }
        }
        any.then_some(report)
    }

    fn hub_descriptor(hub: &SimHub) -> Vec<u8> {
        let ports = hub.ports.len() as u8;
        let characteristics: u16 =
            u16::from(hub.think_time) << 5 | if hub.power_switching { 1 } else { 2 };
        let [c0, c1] = characteristics.to_le_bytes();
        if hub.super_speed {
            vec![12, 0x2a, ports, c0, c1, hub.power_good, 0, 0, 0, 0, 0, 0]
        } else {
            let bitmap = (usize::from(ports) + 8) / 8;
            let mut bytes = vec![
                (7 + 2 * bitmap) as u8,
                0x29,
                ports,
                c0,
                c1,
                hub.power_good,
                0,
            ];
            bytes.extend(core::iter::repeat_n(0, bitmap));
            bytes.extend(core::iter::repeat_n(0xff, bitmap));
            bytes
        }
    }

    fn descriptor(&self, kind_: u8, index: u8) -> Option<Vec<u8>> {
        match kind_ {
            kind::DEVICE => Some(self.descriptor.clone()),
            kind::CONFIGURATION => self.configurations.get(usize::from(index)).cloned(),
            kind::STRING if index == 0 => Some(vec![4, 3, 0x09, 0x04]),
            kind::STRING => Some(vec![10, 3, b'S', 0, b'I', 0, b'M', 0, b'!', 0]),
            _ => None,
        }
    }

    pub fn control(&mut self, setup: Setup) -> Reply {
        self.requests.push(setup);
        if self.faults.silent {
            return Reply::Silent;
        }
        if let Some(n) = self.faults.pull_at
            && self.requests.len() as u32 >= n
        {
            self.pull_me = true;
            return Reply::Silent;
        }
        let descriptor_type = (setup.value >> 8) as u8;
        let index = setup.value as u8;
        let reply = match (setup.request_type, setup.request) {
            (0x80, 6) => {
                if self.faults.stall_descriptor == Some(descriptor_type) {
                    let stall = self.faults.stalls == 0
                        || self
                            .requests
                            .iter()
                            .filter(|r| r.request == 6 && (r.value >> 8) as u8 == descriptor_type)
                            .count() as u32
                            <= self.faults.stalls;
                    if stall {
                        return Reply::Stall;
                    }
                }
                if self.faults.babble_descriptor == Some(descriptor_type) {
                    return Reply::Babble;
                }
                match self.descriptor(descriptor_type, index) {
                    Some(mut bytes) => {
                        if let Some((t, len)) = self.faults.truncate
                            && t == descriptor_type
                        {
                            bytes.truncate(len);
                        }
                        Reply::Data(bytes)
                    }
                    None => Reply::Stall,
                }
            }
            (0x00, 9) => {
                let value = setup.value as u8;
                let known = value == 0 || self.configurations.iter().any(|c| c[5] == value);
                if known {
                    self.configuration = value;
                    self.toggles = 0;
                    Reply::Data(Vec::new())
                } else {
                    Reply::Stall
                }
            }
            (0x80, 0) => Reply::Data(vec![u8::from(self.self_powered), 0]),
            (0x01, 11) => Reply::Data(Vec::new()),
            (0x02, 1) => {
                let address = setup.index as u8;
                let dci = crate::xhci::context::dci(address & 0x0f, address & 0x80 != 0);
                if self.faults.stall_clear_halt & 1 << dci != 0 {
                    Reply::Stall
                } else {
                    if setup.value == 0 {
                        self.halted_endpoints &= !(1 << dci);
                        self.toggles &= !(1 << dci);
                    }
                    Reply::Data(Vec::new())
                }
            }
            (0x23, 1) if self.faults.stall_port_clear => Reply::Stall,
            (0xa1, 0xfe) if self.storage.is_some() => {
                match self.storage.as_ref().and_then(|s| s.max_lun()) {
                    Some(lun) => Reply::Data(vec![lun]),
                    None => Reply::Stall,
                }
            }
            (0x21, 0xff) if self.storage.is_some() => {
                if self.storage.as_mut().is_some_and(|s| s.reset()) {
                    Reply::Data(Vec::new())
                } else {
                    Reply::Stall
                }
            }
            _ => match self.hub.as_mut() {
                Some(hub) if self.configuration != 0 => Self::hub_request(hub, setup),
                _ => Reply::Stall,
            },
        };
        match reply {
            Reply::Data(mut bytes) => {
                bytes.truncate(usize::from(setup.length));
                Reply::Data(bytes)
            }
            other => other,
        }
    }

    fn hub_request(hub: &mut SimHub, setup: Setup) -> Reply {
        let port = usize::from(setup.index).wrapping_sub(1);
        let valid = port < hub.ports.len();
        let kind_ = (setup.value >> 8) as u8;
        match (setup.request_type, setup.request) {
            (0xa0, 6) if kind_ == if hub.super_speed { 0x2a } else { 0x29 } => {
                Reply::Data(Self::hub_descriptor(hub))
            }
            (0xa0, 0) => Reply::Data(vec![0; 4]),
            (0x20, 1) => Reply::Data(Vec::new()),
            (0x23, 8) if !hub.super_speed => {
                hub.tt_clears.push((setup.value, setup.index));
                Reply::Data(Vec::new())
            }
            (0x20, 12) if hub.super_speed => {
                hub.depth = Some(setup.value as u8);
                Reply::Data(Vec::new())
            }
            (0xa3, 0) if valid => Reply::Data(Self::port_status(hub, port).to_vec()),
            (0x23, 3) if valid => {
                let super_speed = hub.super_speed;
                let p = &mut hub.ports[port];
                match setup.value {
                    8 => {
                        if !p.powered {
                            p.powered = true;
                            if p.device.is_some() {
                                p.changes |= 1;
                                p.enabled = super_speed;
                            }
                        }
                    }
                    4 | 28 => {
                        if p.powered && p.device.is_some() {
                            p.enabled = true;
                            p.changes |= if setup.value == 4 { 1 << 4 } else { 1 << 5 };
                        }
                    }
                    _ => {}
                }
                Reply::Data(Vec::new())
            }
            (0x23, 1) if valid => {
                let p = &mut hub.ports[port];
                match setup.value {
                    1 => {
                        p.enabled = false;
                        if let Some(device) = p.device.as_mut() {
                            device.default_state = false;
                        }
                    }
                    16 => p.changes &= !1,
                    17 => p.changes &= !(1 << 1),
                    18 => p.changes &= !(1 << 2),
                    19 => p.changes &= !(1 << 3),
                    20 => p.changes &= !(1 << 4),
                    29 => p.changes &= !(1 << 5),
                    25 => p.changes &= !(1 << 6),
                    26 => p.changes &= !(1 << 7),
                    _ => {}
                }
                Reply::Data(Vec::new())
            }
            _ => Reply::Stall,
        }
    }

    /// Visit this device and every device below it.
    pub fn walk(&self, visit: &mut impl FnMut(&SimDevice)) {
        visit(self);
        if let Some(hub) = &self.hub {
            for p in &hub.ports {
                if let Some(d) = &p.device {
                    d.walk(visit);
                }
            }
        }
    }
}
