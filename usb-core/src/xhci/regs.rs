//! The register file in BAR0: capability registers at its base, then the
//! operational, runtime and doorbell sets at offsets the capability registers
//! name (xHCI 1.2 §5).

use super::bus::RegisterBus;

pub const CAPLENGTH: usize = 0x00;
pub const HCSPARAMS1: usize = 0x04;
pub const HCSPARAMS2: usize = 0x08;
pub const HCCPARAMS1: usize = 0x10;
pub const DBOFF: usize = 0x14;
pub const RTSOFF: usize = 0x18;

pub const USBCMD: usize = 0x00;
pub const USBSTS: usize = 0x04;
pub const PAGESIZE: usize = 0x08;
pub const CRCR: usize = 0x18;
pub const DCBAAP: usize = 0x30;
pub const CONFIG: usize = 0x38;
const PORT_SETS: usize = 0x400;
const PORT_SET_BYTES: usize = 0x10;

const INTERRUPTER_SETS: usize = 0x20;
const INTERRUPTER_SET_BYTES: usize = 0x20;
pub const IMAN: usize = 0x00;
pub const IMOD: usize = 0x04;
pub const ERSTSZ: usize = 0x08;
pub const ERSTBA: usize = 0x10;
pub const ERDP: usize = 0x18;

pub const CMD_RUN: u32 = 1 << 0;
pub const CMD_RESET: u32 = 1 << 1;
pub const CMD_INTE: u32 = 1 << 2;
pub const CMD_HSEE: u32 = 1 << 3;

pub const STS_HALTED: u32 = 1 << 0;
pub const STS_HOST_SYSTEM_ERROR: u32 = 1 << 2;
pub const STS_EVENT_INTERRUPT: u32 = 1 << 3;
pub const STS_PORT_CHANGE: u32 = 1 << 4;
pub const STS_NOT_READY: u32 = 1 << 11;
pub const STS_CONTROLLER_ERROR: u32 = 1 << 12;
/// The status bits a write of one clears (§5.4.2).
pub const STS_RW1C: u32 = STS_HOST_SYSTEM_ERROR | STS_EVENT_INTERRUPT | STS_PORT_CHANGE | 1 << 10;

pub const IMAN_PENDING: u32 = 1 << 0;
pub const IMAN_ENABLE: u32 = 1 << 1;
pub const ERDP_BUSY: u64 = 1 << 3;

/// The interrupter moderation interval in 250 ns units: at most one
/// interrupt every 40 µs.
pub const IMOD_INTERVAL: u32 = 160;

/// The capability registers, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub cap_length: u8,
    /// HCIVERSION as BCD: 0x0110 is xHCI 1.1.
    pub version: u16,
    pub max_slots: u8,
    pub max_interrupters: u16,
    pub max_ports: u8,
    pub scratchpads: u16,
    pub ac64: bool,
    pub context_64: bool,
    /// The first extended capability, in dwords from the base; 0 for none.
    pub xecp: u16,
    pub dboff: u32,
    pub rtsoff: u32,
    /// PAGESIZE: bit n set means pages of 2^(n+12) bytes.
    pub page_sizes: u16,
}

/// Why a controller's registers cannot describe a controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// Every register reads all ones: the function is gone or not decoding.
    Absent,
    /// CAPLENGTH would leave the operational registers' 64-bit ones off a
    /// qword boundary.
    Misaligned,
    /// A register set or a port lies past the end of BAR0.
    OutOfBar,
    NoPorts,
    NoSlots,
    NoInterrupters,
}

/// Why a well-formed controller is not driven.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decline {
    /// Rings and contexts come from anywhere in memory, and the frame
    /// allocator has no below-4 GiB constraint to ask for.
    No64BitAddressing,
    TooManyScratchpads(u16),
    /// Every ring, context and buffer is laid out in 4 KiB pages.
    No4KPages,
}

/// The most scratchpad buffers one page of pointers lists.
pub const MAX_SCRATCHPADS: u16 = 512;

fn bits(value: u32, shift: u32, width: u32) -> u32 {
    (value >> shift) & ((1 << width) - 1)
}

impl Capabilities {
    /// Read the capability registers and PAGESIZE, which writes nothing, and
    /// check that every register set they place lies inside `bar_len` bytes.
    pub fn read<B: RegisterBus>(bus: &mut B, bar_len: usize) -> Result<Self, Malformed> {
        if bar_len < 0x20 {
            return Err(Malformed::OutOfBar);
        }
        let dword0 = bus.read32(CAPLENGTH);
        let params1 = bus.read32(HCSPARAMS1);
        let params2 = bus.read32(HCSPARAMS2);
        let cparams1 = bus.read32(HCCPARAMS1);
        let dboff = bus.read32(DBOFF);
        let rtsoff = bus.read32(RTSOFF);
        if [dword0, params1, params2, cparams1, dboff, rtsoff].contains(&u32::MAX) {
            return Err(Malformed::Absent);
        }
        let cap_length = dword0 as u8;
        if usize::from(cap_length) < 0x20 || usize::from(cap_length) + PAGESIZE + 4 > bar_len {
            return Err(Malformed::OutOfBar);
        }
        if !cap_length.is_multiple_of(8) {
            return Err(Malformed::Misaligned);
        }
        let page_sizes = bus.read32(usize::from(cap_length) + PAGESIZE) as u16;
        let caps = Self {
            cap_length,
            version: (dword0 >> 16) as u16,
            max_slots: bits(params1, 0, 8) as u8,
            max_interrupters: bits(params1, 8, 11) as u16,
            max_ports: bits(params1, 24, 8) as u8,
            scratchpads: (bits(params2, 21, 5) << 5 | bits(params2, 27, 5)) as u16,
            ac64: cparams1 & 1 != 0,
            context_64: cparams1 & 1 << 2 != 0,
            xecp: (cparams1 >> 16) as u16,
            dboff: dboff & !0x3,
            rtsoff: rtsoff & !0x1f,
            page_sizes,
        };
        caps.check(bar_len)?;
        Ok(caps)
    }

    fn check(&self, bar_len: usize) -> Result<(), Malformed> {
        if self.max_ports == 0 {
            return Err(Malformed::NoPorts);
        }
        if self.max_slots == 0 {
            return Err(Malformed::NoSlots);
        }
        if self.max_interrupters == 0 {
            return Err(Malformed::NoInterrupters);
        }
        let layout = self.layout();
        let ends = [
            layout.port(self.max_ports) + PORT_SET_BYTES,
            layout.interrupter(0) + INTERRUPTER_SET_BYTES,
            layout.doorbell(self.max_slots) + 4,
            layout.extended_capabilities().map_or(0, |at| at + 4),
        ];
        if ends.iter().any(|&end| end > bar_len) {
            return Err(Malformed::OutOfBar);
        }
        Ok(())
    }

    /// What keeps this driver from running the controller, if anything.
    pub fn decline(&self) -> Option<Decline> {
        if !self.ac64 {
            return Some(Decline::No64BitAddressing);
        }
        if self.scratchpads > MAX_SCRATCHPADS {
            return Some(Decline::TooManyScratchpads(self.scratchpads));
        }
        if self.page_sizes & 1 == 0 {
            return Some(Decline::No4KPages);
        }
        None
    }

    pub fn context_bytes(&self) -> usize {
        if self.context_64 { 64 } else { 32 }
    }

    pub fn layout(&self) -> Layout {
        Layout {
            operational: usize::from(self.cap_length),
            runtime: self.rtsoff as usize,
            doorbells: self.dboff as usize,
            xecp: usize::from(self.xecp) * 4,
            max_ports: self.max_ports,
        }
    }

    /// `(major, minor)`.
    pub fn version_parts(&self) -> (u8, u8) {
        let bcd = |b: u8| (b >> 4) * 10 + (b & 0xf);
        (bcd((self.version >> 8) as u8), bcd(self.version as u8))
    }
}

/// Byte offsets of the register sets within BAR0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub operational: usize,
    pub runtime: usize,
    pub doorbells: usize,
    xecp: usize,
    max_ports: u8,
}

impl Layout {
    pub fn op(&self, reg: usize) -> usize {
        self.operational + reg
    }

    /// PORTSC of root port `port`, numbered from 1 as the specification does.
    pub fn port(&self, port: u8) -> usize {
        self.operational + PORT_SETS + PORT_SET_BYTES * usize::from(port).saturating_sub(1)
    }

    pub fn max_ports(&self) -> u8 {
        self.max_ports
    }

    pub fn interrupter(&self, n: u16) -> usize {
        self.runtime + INTERRUPTER_SETS + INTERRUPTER_SET_BYTES * usize::from(n)
    }

    pub fn doorbell(&self, n: u8) -> usize {
        self.doorbells + 4 * usize::from(n)
    }

    pub fn extended_capabilities(&self) -> Option<usize> {
        (self.xecp != 0).then_some(self.xecp)
    }
}

/// PORTSC (§5.4.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortSc(pub u32);

pub const PORT_CONNECTED: u32 = 1 << 0;
pub const PORT_ENABLED: u32 = 1 << 1;
pub const PORT_OVER_CURRENT: u32 = 1 << 3;
pub const PORT_RESET: u32 = 1 << 4;
pub const PORT_POWER: u32 = 1 << 9;
pub const PORT_CONNECT_CHANGE: u32 = 1 << 17;
pub const PORT_OVER_CURRENT_CHANGE: u32 = 1 << 20;
/// CSC, PEC, WRC, OCC, PRC, PLC and CEC: written as one to clear.
pub const PORT_CHANGES: u32 = 0x7f << 17;
/// The bits a write must carry as read to leave them as they are: port power,
/// the indicator and the wake enables. Every other bit is written as zero,
/// which leaves PED and the change bits alone and starts no reset.
pub const PORT_PRESERVE: u32 = PORT_POWER | 0b11 << 14 | 0b111 << 25;

impl PortSc {
    pub fn connected(self) -> bool {
        self.0 & PORT_CONNECTED != 0
    }

    pub fn enabled(self) -> bool {
        self.0 & PORT_ENABLED != 0
    }

    pub fn powered(self) -> bool {
        self.0 & PORT_POWER != 0
    }

    pub fn over_current(self) -> bool {
        self.0 & PORT_OVER_CURRENT != 0
    }

    pub fn changes(self) -> u32 {
        self.0 & PORT_CHANGES
    }

    pub fn connect_changed(self) -> bool {
        self.0 & PORT_CONNECT_CHANGE != 0
    }

    pub fn over_current_changed(self) -> bool {
        self.0 & PORT_OVER_CURRENT_CHANGE != 0
    }

    /// The Protocol Speed ID of what is attached.
    pub fn speed(self) -> u8 {
        bits(self.0, 10, 4) as u8
    }

    /// The write that clears `changes` and touches nothing else.
    pub fn acknowledge(self, changes: u32) -> u32 {
        (self.0 & PORT_PRESERVE) | (changes & PORT_CHANGES)
    }

    /// The write that powers the port and touches nothing else.
    pub fn power_on(self) -> u32 {
        (self.0 & PORT_PRESERVE) | PORT_POWER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_writes_never_disable_reset_or_drop_power() {
        let enabled_with_changes = PortSc(
            PORT_CONNECTED | PORT_ENABLED | PORT_POWER | 3 << 10 | PORT_CONNECT_CHANGE | 1 << 21,
        );
        let ack = enabled_with_changes.acknowledge(enabled_with_changes.changes());
        assert_eq!(ack & (PORT_ENABLED | PORT_RESET | 1 << 16 | 1 << 31), 0);
        assert_eq!(ack & PORT_POWER, PORT_POWER);
        assert_eq!(ack & PORT_CHANGES, PORT_CONNECT_CHANGE | 1 << 21);
        let off = PortSc(PORT_CONNECTED | PORT_CONNECT_CHANGE);
        assert_eq!(off.power_on(), PORT_POWER);
        assert_eq!(enabled_with_changes.speed(), 3);
    }

    #[test]
    fn layout_places_each_set() {
        let caps = Capabilities {
            cap_length: 0x40,
            version: 0x0120,
            max_slots: 64,
            max_interrupters: 8,
            max_ports: 8,
            scratchpads: 0,
            ac64: true,
            context_64: false,
            xecp: 0x8000 / 4,
            dboff: 0x2000,
            rtsoff: 0x1000,
            page_sizes: 1,
        };
        let layout = caps.layout();
        assert_eq!(layout.op(USBSTS), 0x44);
        assert_eq!(layout.port(1), 0x440);
        assert_eq!(layout.port(8), 0x4b0);
        assert_eq!(layout.interrupter(0) + ERDP, 0x1038);
        assert_eq!(layout.doorbell(0), 0x2000);
        assert_eq!(layout.doorbell(3), 0x200c);
        assert_eq!(layout.extended_capabilities(), Some(0x8000));
        assert_eq!(caps.version_parts(), (1, 20));
        assert_eq!(caps.check(0x8004), Ok(()));
        assert_eq!(caps.check(0x8000), Err(Malformed::OutOfBar));
    }
}
