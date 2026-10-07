//! The extended capability list (§7): USB Legacy Support, through which the
//! firmware hands the controller over, and the Supported Protocol
//! capabilities that say which root ports speak USB 2 and which USB 3.

use super::bus::RegisterBus;
use super::regs::Layout;

pub const ID_LEGACY_SUPPORT: u8 = 1;
pub const ID_SUPPORTED_PROTOCOL: u8 = 2;

/// A list longer than this is a loop or garbage, not capabilities.
const MAX_CAPABILITIES: usize = 64;

/// One entry: its byte offset in BAR0 and the header dword there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtCap {
    pub offset: usize,
    pub header: u32,
}

impl ExtCap {
    pub fn id(&self) -> u8 {
        self.header as u8
    }
}

/// Walk the list, which `next` pointers chain forward in dwords. A pointer
/// past `bar_len` ends it, as does an all-ones header.
pub fn walk<B: RegisterBus>(
    bus: &mut B,
    layout: &Layout,
    bar_len: usize,
    mut visit: impl FnMut(&mut B, ExtCap),
) {
    let Some(mut offset) = layout.extended_capabilities() else {
        return;
    };
    for _ in 0..MAX_CAPABILITIES {
        if offset + 4 > bar_len {
            return;
        }
        let header = bus.read32(offset);
        if header == u32::MAX {
            return;
        }
        visit(bus, ExtCap { offset, header });
        let next = ((header >> 8) & 0xff) as usize;
        if next == 0 {
            return;
        }
        offset += next * 4;
    }
}

/// USBLEGSUP and USBLEGCTLSTS (§7.1).
pub mod legacy {
    pub const BIOS_OWNED: u32 = 1 << 16;
    pub const BIOS_OWNED_BYTE: usize = 2;
    pub const OS_OWNED_BYTE: usize = 3;
    pub const CONTROL: usize = 4;
    /// USB SMI, SMI on Host System Error, on OS Ownership, on PCI Command
    /// and on BAR.
    pub const SMI_ENABLES: u32 = 1 | 1 << 4 | 1 << 13 | 1 << 14 | 1 << 15;
    /// SMI on OS Ownership Change, on PCI Command and on BAR: written as one
    /// to clear.
    pub const SMI_EVENTS: u32 = 0b111 << 29;
    /// The bits a write carries as read; the rest are enables, read-only or
    /// reserved, and written as zero.
    pub const CONTROL_PRESERVE: u32 = 0x000e_1fee;
}

/// The USB major and minor revision a range of root ports speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Protocol {
    pub major: u8,
    pub minor: u8,
    /// The first root port, numbered from 1.
    pub first_port: u8,
    pub ports: u8,
    /// Its Protocol Speed IDs, in [`Protocols`]' shared table.
    psi_start: u8,
    psi_count: u8,
}

/// Room for two protocols' Speed ID tables of 15 entries each.
const MAX_PSI: usize = 30;
const NAME_USB: u32 = u32::from_le_bytes(*b"USB ");

impl Protocol {
    pub fn covers(&self, port: u8) -> bool {
        port >= self.first_port
            && u16::from(port) < u16::from(self.first_port) + u16::from(self.ports)
    }

    pub fn last_port(&self) -> u8 {
        (u16::from(self.first_port) + u16::from(self.ports) - 1) as u8
    }

    /// The revision as decimal `major.minor`. Both fields are BCD, though
    /// some controllers write a minor revision of 1 for 0x10.
    pub fn revision(&self) -> (u8, u8) {
        let bcd = |b: u8| (b >> 4) * 10 + (b & 0xf);
        let minor = if self.minor < 0x10 {
            self.minor
        } else {
            bcd(self.minor) / 10
        };
        (bcd(self.major), minor)
    }
}

/// A port's link speed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Speed {
    pub bits_per_second: u64,
    pub name: Option<&'static str>,
}

impl Speed {
    fn default_for(major: u8, psiv: u8) -> Option<Self> {
        let (bits_per_second, name) = match (major, psiv) {
            (2, 1) => (12_000_000, "full speed"),
            (2, 2) => (1_500_000, "low speed"),
            (2, 3) => (480_000_000, "high speed"),
            (3, 4) => (5_000_000_000, "SuperSpeed"),
            (3, 5) => (10_000_000_000, "SuperSpeedPlus Gen 2x1"),
            (3, 6) => (10_000_000_000, "SuperSpeedPlus Gen 1x2"),
            (3, 7) => (20_000_000_000, "SuperSpeedPlus Gen 2x2"),
            _ => return None,
        };
        Some(Self {
            bits_per_second,
            name: Some(name),
        })
    }
}

const MAX_PROTOCOLS: usize = 8;

/// Every Supported Protocol capability a controller lists, as far as eight.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Protocols {
    entries: [Protocol; MAX_PROTOCOLS],
    len: usize,
    psi: [u32; MAX_PSI],
    psi_len: usize,
}

impl Protocols {
    pub fn iter(&self) -> impl Iterator<Item = &Protocol> {
        self.entries[..self.len].iter()
    }

    pub fn of_port(&self, port: u8) -> Option<&Protocol> {
        self.iter().find(|p| p.covers(port))
    }

    /// What Protocol Speed ID `psiv` names on root port `port`: its
    /// protocol's own Speed ID table when it carries one, else the defaults
    /// of §7.2.2.1.
    pub fn speed(&self, port: u8, psiv: u8) -> Option<Speed> {
        let protocol = self.of_port(port)?;
        if protocol.psi_count == 0 {
            return Speed::default_for(protocol.major, psiv);
        }
        let start = usize::from(protocol.psi_start);
        let psi = self.psi[start..start + usize::from(protocol.psi_count)]
            .iter()
            .find(|&&psi| psi & 0xf == u32::from(psiv))?;
        let exponent = (psi >> 4) & 0b11;
        let mantissa = psi >> 16;
        Some(Speed {
            bits_per_second: u64::from(mantissa) * 1000u64.pow(exponent),
            name: None,
        })
    }

    /// Record the capability at `cap`, keeping only ports `1..=max_ports`
    /// that no earlier capability claimed: a controller that lists a port
    /// twice or past its end describes nothing there.
    fn add<B: RegisterBus>(&mut self, bus: &mut B, cap: ExtCap, max_ports: u8, bar_len: usize) {
        if self.len == MAX_PROTOCOLS || cap.offset + 16 > bar_len {
            return;
        }
        let name = bus.read32(cap.offset + 4);
        let ports = bus.read32(cap.offset + 8);
        let first_port = ports as u8;
        let count = (ports >> 8) as u8;
        let last = u16::from(first_port) + u16::from(count);
        if name != NAME_USB || first_port == 0 || count == 0 || last > u16::from(max_ports) + 1 {
            return;
        }
        if (u16::from(first_port)..last).any(|p| self.of_port(p as u8).is_some()) {
            return;
        }
        let listed = ((ports >> 28) & 0xf) as usize;
        let mut protocol = Protocol {
            major: (cap.header >> 24) as u8,
            minor: (cap.header >> 16) as u8,
            first_port,
            ports: count,
            psi_start: self.psi_len as u8,
            psi_count: 0,
        };
        for i in 0..listed.min(MAX_PSI - self.psi_len) {
            let at = cap.offset + 16 + 4 * i;
            if at + 4 > bar_len {
                break;
            }
            self.psi[self.psi_len] = bus.read32(at);
            self.psi_len += 1;
            protocol.psi_count += 1;
        }
        self.entries[self.len] = protocol;
        self.len += 1;
    }
}

/// What the list says about the controller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// USBLEGSUP's offset, when the controller has USB Legacy Support.
    pub legacy: Option<usize>,
    pub protocols: Protocols,
}

pub fn find<B: RegisterBus>(bus: &mut B, layout: &Layout, bar_len: usize) -> Found {
    let mut found = Found::default();
    walk(bus, layout, bar_len, |bus, cap| match cap.id() {
        ID_LEGACY_SUPPORT if found.legacy.is_none() && cap.offset + 8 <= bar_len => {
            found.legacy = Some(cap.offset);
        }
        ID_SUPPORTED_PROTOCOL => found.protocols.add(bus, cap, layout.max_ports(), bar_len),
        _ => {}
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_speed_ids_follow_the_protocol() {
        let mut protocols = Protocols::default();
        protocols.entries[0] = Protocol {
            major: 2,
            first_port: 1,
            ports: 4,
            ..Protocol::default()
        };
        protocols.entries[1] = Protocol {
            major: 3,
            first_port: 5,
            ports: 4,
            ..Protocol::default()
        };
        protocols.len = 2;
        assert_eq!(protocols.speed(1, 3).unwrap().name, Some("high speed"));
        assert_eq!(protocols.speed(4, 2).unwrap().bits_per_second, 1_500_000);
        assert_eq!(protocols.speed(4, 4), None);
        assert_eq!(protocols.speed(5, 4).unwrap().name, Some("SuperSpeed"));
        assert_eq!(protocols.speed(9, 4), None);
        let usb3 = protocols.of_port(8).unwrap();
        assert!(usb3.covers(5) && usb3.covers(8) && !usb3.covers(9) && !usb3.covers(4));
        assert_eq!(usb3.last_port(), 8);
        let revision = |major, minor| {
            Protocol {
                major,
                minor,
                ..Protocol::default()
            }
            .revision()
        };
        assert_eq!(revision(0x03, 0x00), (3, 0));
        assert_eq!(revision(0x03, 0x20), (3, 2));
        assert_eq!(revision(0x03, 0x01), (3, 1));
        assert_eq!(revision(0x02, 0x00), (2, 0));
    }

    #[test]
    fn a_speed_id_table_replaces_the_defaults() {
        let mut protocols = Protocols::default();
        protocols.entries[0] = Protocol {
            major: 3,
            first_port: 1,
            ports: 1,
            psi_start: 1,
            psi_count: 2,
            ..Protocol::default()
        };
        protocols.len = 1;
        protocols.psi[1] = 4 | 3 << 4 | 5 << 16;
        protocols.psi[2] = 7 | 2 << 4 | 2500 << 16;
        protocols.psi_len = 3;
        assert_eq!(
            protocols.speed(1, 4).unwrap().bits_per_second,
            5_000_000_000
        );
        assert_eq!(
            protocols.speed(1, 7).unwrap().bits_per_second,
            2_500_000_000
        );
        assert_eq!(protocols.speed(1, 4).unwrap().name, None);
        assert_eq!(protocols.speed(1, 5), None);
    }
}
