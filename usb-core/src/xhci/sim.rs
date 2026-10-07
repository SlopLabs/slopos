//! A simulated xHCI controller behind [`RegisterBus`]: the register file, the
//! firmware's side of USB Legacy Support, a command ring it consumes and an
//! event ring it produces into, in memory shared with the pages the driver
//! side owns, and root ports a test plugs and pulls.

use super::bus::RegisterBus;
use super::memory::{DmaPage, PAGE_SIZE};
use super::regs::*;
use super::trb::{Trb, kind};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

pub const BAR_LEN: usize = 0x4000;
const OP: usize = 0x40;
const RT: usize = 0x1000;
const DB: usize = 0x2000;
const XECP: usize = 0x3000;

/// Physical memory both sides reach: the driver through [`SimPage`]s, the
/// controller by DMA.
#[derive(Clone, Default)]
pub struct Memory(Rc<RefCell<MemoryInner>>);

#[derive(Default)]
struct MemoryInner {
    pages: BTreeMap<u64, Vec<u8>>,
    allocated: u64,
}

/// Pages are placed above 4 GiB, so a register given only its low half
/// points nowhere.
const FIRST_PAGE: u64 = 0x1_0000_0000;

impl Memory {
    pub fn page(&self) -> SimPage {
        let mut inner = self.0.borrow_mut();
        let phys = FIRST_PAGE + inner.allocated * PAGE_SIZE as u64;
        inner.allocated += 1;
        inner.pages.insert(phys, vec![0; PAGE_SIZE]);
        SimPage {
            phys,
            mem: self.clone(),
            fences: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn locate(&self, phys: u64, len: usize) -> Option<(u64, usize)> {
        let page = phys & !(PAGE_SIZE as u64 - 1);
        let offset = (phys - page) as usize;
        (offset + len <= PAGE_SIZE && self.0.borrow().pages.contains_key(&page))
            .then_some((page, offset))
    }

    pub fn read32(&self, phys: u64) -> u32 {
        let (page, offset) = self.locate(phys, 4).expect("DMA read outside memory");
        let inner = self.0.borrow();
        let b = &inner.pages[&page][offset..offset + 4];
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    pub fn read64(&self, phys: u64) -> u64 {
        u64::from(self.read32(phys)) | u64::from(self.read32(phys + 4)) << 32
    }

    pub fn write32(&self, phys: u64, value: u32) {
        let (page, offset) = self.locate(phys, 4).expect("DMA write outside memory");
        let mut inner = self.0.borrow_mut();
        let bytes = inner.pages.get_mut(&page).unwrap();
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    pub fn write64(&self, phys: u64, value: u64) {
        self.write32(phys, value as u32);
        self.write32(phys + 4, (value >> 32) as u32);
    }

    pub fn is_page(&self, phys: u64) -> bool {
        self.0.borrow().pages.contains_key(&phys)
    }
}

/// What a page saw, in order: its writes, by offset, and its fences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageOp {
    Read(usize),
    Write(usize),
    Release,
    Acquire,
}

pub struct SimPage {
    phys: u64,
    mem: Memory,
    pub fences: Rc<RefCell<Vec<PageOp>>>,
}

impl DmaPage for SimPage {
    fn phys(&self) -> u64 {
        self.phys
    }
    fn read32(&self, offset: usize) -> u32 {
        self.fences.borrow_mut().push(PageOp::Read(offset));
        self.mem.read32(self.phys + offset as u64)
    }
    fn write32(&mut self, offset: usize, value: u32) {
        self.fences.borrow_mut().push(PageOp::Write(offset));
        self.mem.write32(self.phys + offset as u64, value);
    }
    fn write64(&mut self, offset: usize, value: u64) {
        self.fences.borrow_mut().push(PageOp::Write(offset));
        self.mem.write64(self.phys + offset as u64, value);
    }
    fn acquire(&self) {
        self.fences.borrow_mut().push(PageOp::Acquire);
    }
    fn release(&self) {
        self.fences.borrow_mut().push(PageOp::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read(usize, u32),
    Write8(usize, u8),
    Write32(usize, u32),
    Write64(usize, u64),
    BusMaster(bool),
    Delay(u32),
}

/// A condition the simulated controller never reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stuck {
    Ready,
    Halt,
    Reset,
    Run,
    /// The controller fetches no command, so each one submitted stays
    /// outstanding.
    Commands,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Legacy {
    /// The firmware clears BIOS Owned this long after it is asked.
    ReleasesAfter(u64),
    NeverLetsGo,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub ac64: bool,
    pub context_64: bool,
    pub scratchpads: u16,
    pub page_sizes: u16,
    pub port_power_control: bool,
    pub legacy: Option<Legacy>,
    /// (major, minor, first port, ports), in list order.
    pub protocols: Vec<(u8, u8, u8, u8)>,
    /// Ports raise an event only once all their change bits are clear, and
    /// a run raises one for each port with a change pending (§4.19.2). When
    /// off, as QEMU's model does, each change raises one while running, and
    /// a run raises none.
    pub specification_port_events: bool,
    pub max_slots: u8,
}

impl Config {
    /// What QEMU's `qemu-xhci` reports with `p2=4,p3=2`.
    pub fn qemu() -> Self {
        Self {
            ac64: true,
            context_64: false,
            scratchpads: 0,
            page_sizes: 1,
            port_power_control: false,
            legacy: None,
            protocols: vec![(0x03, 0x00, 1, 2), (0x02, 0x00, 3, 4)],
            specification_port_events: false,
            max_slots: 64,
        }
    }

    /// A laptop-shaped controller: 64-byte contexts, scratchpads, USB
    /// Legacy Support, switched port power and the specification's events.
    pub fn intel() -> Self {
        Self {
            ac64: true,
            context_64: true,
            scratchpads: 300,
            page_sizes: 1,
            port_power_control: true,
            legacy: Some(Legacy::ReleasesAfter(2000)),
            protocols: vec![(0x02, 0x00, 1, 4), (0x03, 0x10, 5, 2)],
            specification_port_events: true,
            max_slots: 32,
        }
    }

    fn ports(&self) -> u8 {
        self.protocols
            .iter()
            .map(|&(_, _, first, count)| first + count - 1)
            .max()
            .unwrap_or(0)
    }

    fn usb3(&self, port: u8) -> bool {
        self.protocols
            .iter()
            .any(|&(major, _, first, count)| major == 3 && port >= first && port < first + count)
    }
}

pub struct SimController {
    pub config: Config,
    pub mem: Memory,
    pub log: Vec<Op>,
    pub stuck: Option<Stuck>,
    pub gone: bool,
    now_us: u64,
    usbcmd: u32,
    halted: bool,
    host_system_error: bool,
    controller_error: bool,
    event_interrupt: bool,
    reset_done_at: Option<u64>,
    reset_written_at: Option<u64>,
    ready_at: u64,
    config_reg: u32,
    dcbaap: u64,
    crcr: u64,
    iman: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    command: u64,
    command_cycle: bool,
    segment: u64,
    segment_trbs: u16,
    enqueue: u16,
    event_cycle: bool,
    ports: Vec<u32>,
    /// The Protocol Speed ID of the device plugged into each port, which
    /// connects once the port has power.
    devices: Vec<Option<u8>>,
    /// A port whose device is pulled, or plugged at `Some` speed, just after
    /// the next read of its PORTSC.
    pub race: Option<(u8, Option<u8>)>,
    /// A port whose device is pulled or plugged back after every write to
    /// its PORTSC.
    pub flapping: Option<u8>,
    bios_owned: bool,
    os_owned: bool,
    bios_release_at: Option<u64>,
    legacy_control: u32,
    pub bus_master: bool,
    /// Every violation of the rules a driver must keep, by what it was.
    pub violations: Vec<&'static str>,
    /// MSI messages the controller would have sent.
    pub interrupts: u32,
    pub dropped_events: u32,
}

const LEGACY_CONTROL_RESET: u32 = 0xe000_e011;
/// PORTSC's link state while a port waits for a device (§5.4.8).
const RX_DETECT: u32 = 5 << 5;

impl SimController {
    /// A controller as the firmware leaves it: running, its ports powered.
    pub fn new(config: Config) -> Self {
        let ports = vec![PORT_POWER | RX_DETECT; usize::from(config.ports())];
        let devices = vec![None; usize::from(config.ports())];
        let bios_owned = config.legacy.is_some();
        Self {
            config,
            mem: Memory::default(),
            log: Vec::new(),
            stuck: None,
            gone: false,
            now_us: 0,
            usbcmd: CMD_RUN,
            halted: false,
            host_system_error: false,
            controller_error: false,
            event_interrupt: false,
            reset_done_at: None,
            reset_written_at: None,
            ready_at: 0,
            config_reg: 0,
            dcbaap: 0,
            crcr: 0,
            iman: 0,
            erstsz: 0,
            erstba: 0,
            erdp: 0,
            command: 0,
            command_cycle: true,
            segment: 0,
            segment_trbs: 0,
            enqueue: 0,
            event_cycle: true,
            ports,
            devices,
            race: None,
            flapping: None,
            bios_owned,
            os_owned: false,
            bios_release_at: None,
            legacy_control: LEGACY_CONTROL_RESET,
            bus_master: true,
            violations: Vec::new(),
            interrupts: 0,
            dropped_events: 0,
        }
    }

    pub fn running(&self) -> bool {
        !self.halted && self.usbcmd & CMD_RUN != 0
    }

    pub fn halted(&self) -> bool {
        self.halted
    }

    pub fn bios_owned(&self) -> bool {
        self.bios_owned
    }

    pub fn os_owned(&self) -> bool {
        self.os_owned
    }

    pub fn legacy_control(&self) -> u32 {
        self.legacy_control
    }

    pub fn dcbaap(&self) -> u64 {
        self.dcbaap
    }

    pub fn config_reg(&self) -> u32 {
        self.config_reg
    }

    pub fn portsc(&self, port: u8) -> u32 {
        self.ports[usize::from(port) - 1]
    }

    fn resetting(&self) -> bool {
        self.reset_done_at.is_some_and(|at| self.now_us < at)
    }

    fn not_ready(&self) -> bool {
        self.resetting() || self.now_us < self.ready_at || self.stuck == Some(Stuck::Ready)
    }

    /// Power the controller up not ready for `us`.
    pub fn not_ready_for(&mut self, us: u64) {
        self.ready_at = self.now_us + us;
    }

    fn check_access(&mut self) {
        if let Some(at) = self.reset_written_at
            && self.now_us < at + 1000
        {
            self.violations
                .push("register touched within 1 ms of HCRST");
        }
    }

    fn legacy_header(&self) -> u32 {
        let next = if self.config.protocols.is_empty() {
            0
        } else {
            4
        };
        1 | next << 8 | u32::from(self.bios_owned) << 16 | u32::from(self.os_owned) << 24
    }

    fn ext_cap_read(&mut self, offset: usize) -> u32 {
        let protocols_at = XECP + if self.config.legacy.is_some() { 16 } else { 0 };
        if self.config.legacy.is_some() && offset < protocols_at {
            if self.bios_release_at.is_some_and(|at| self.now_us >= at) {
                self.bios_owned = false;
            }
            return match offset - XECP {
                0 => self.legacy_header(),
                4 => self.legacy_control,
                _ => 0,
            };
        }
        let index = (offset - protocols_at) / 16;
        let Some(&(major, minor, first, count)) = self.config.protocols.get(index) else {
            return 0;
        };
        let last = index + 1 == self.config.protocols.len();
        match (offset - protocols_at) % 16 {
            0 => {
                let next = if last { 0 } else { 4 };
                2 | next << 8 | u32::from(minor) << 16 | u32::from(major) << 24
            }
            4 => u32::from_le_bytes(*b"USB "),
            8 => u32::from(first) | u32::from(count) << 8,
            _ => 0,
        }
    }

    fn read_register(&mut self, offset: usize) -> u32 {
        if self.gone {
            return u32::MAX;
        }
        let params2 = u32::from(self.config.scratchpads >> 5) << 21
            | u32::from(self.config.scratchpads & 0x1f) << 27;
        let cparams1 = u32::from(self.config.ac64)
            | u32::from(self.config.context_64) << 2
            | u32::from(self.config.port_power_control) << 3
            | ((XECP / 4) as u32) << 16;
        let port_set = OP + 0x400;
        let port_end = port_set + 0x10 * self.ports.len();
        match offset {
            CAPLENGTH => OP as u32 | 0x0120 << 16,
            HCSPARAMS1 => {
                u32::from(self.config.ports()) << 24 | 16 << 8 | u32::from(self.config.max_slots)
            }
            HCSPARAMS2 => params2,
            HCCPARAMS1 => cparams1,
            DBOFF => DB as u32,
            RTSOFF => RT as u32,
            o if o == OP + USBCMD => {
                if self.resetting() {
                    self.usbcmd | CMD_RESET
                } else {
                    self.usbcmd
                }
            }
            o if o == OP + USBSTS => [
                (self.halted, STS_HALTED),
                (self.host_system_error, STS_HOST_SYSTEM_ERROR),
                (self.event_interrupt, STS_EVENT_INTERRUPT),
                (self.not_ready(), STS_NOT_READY),
                (self.controller_error, STS_CONTROLLER_ERROR),
            ]
            .iter()
            .filter(|(set, _)| *set)
            .fold(0, |sts, (_, bit)| sts | bit),
            o if o == OP + PAGESIZE => u32::from(self.config.page_sizes),
            o if o == OP + CONFIG => self.config_reg,
            o if (port_set..port_end).contains(&o) && (o - port_set).is_multiple_of(0x10) => {
                self.ports[(o - port_set) / 0x10]
            }
            o if o == RT + 0x20 + IMAN => self.iman,
            o if o >= XECP => self.ext_cap_read(o),
            _ => 0,
        }
    }

    fn reset(&mut self) {
        if !self.halted {
            self.violations.push("HCRST set while running");
        }
        self.reset_written_at = Some(self.now_us);
        if self.stuck == Some(Stuck::Reset) {
            self.reset_done_at = Some(u64::MAX);
        } else {
            self.reset_done_at = Some(self.now_us + 1500);
        }
        self.usbcmd = 0;
        self.halted = true;
        self.host_system_error = false;
        self.controller_error = false;
        self.event_interrupt = false;
        self.config_reg = 0;
        self.dcbaap = 0;
        self.crcr = 0;
        self.iman = 0;
        self.erstsz = 0;
        self.erstba = 0;
        self.erdp = 0;
        self.segment = 0;
        self.segment_trbs = 0;
        let power = if self.config.port_power_control {
            0
        } else {
            PORT_POWER
        };
        for port in 1..=self.config.ports() {
            self.ports[usize::from(port) - 1] = power | RX_DETECT;
            if !self.config.specification_port_events {
                self.ports[usize::from(port) - 1] |= PORT_CONNECT_CHANGE;
            }
            if power != 0
                && let Some(speed) = self.devices[usize::from(port) - 1]
            {
                self.ports[usize::from(port) - 1] |= self.connection(port, speed);
            }
        }
    }

    /// The PORTSC bits a device at `speed` sets once it connects: USB 3 ports
    /// enable themselves; USB 2 ports wait for a reset.
    fn connection(&self, port: u8, speed: u8) -> u32 {
        let enabled = if self.config.usb3(port) {
            PORT_ENABLED
        } else {
            0
        };
        PORT_CONNECTED | enabled | u32::from(speed) << 10 | PORT_CONNECT_CHANGE
    }

    fn write_command(&mut self, value: u32) {
        if value & CMD_RESET != 0 {
            return self.reset();
        }
        let was_running = self.usbcmd & CMD_RUN != 0;
        self.usbcmd = value;
        if value & CMD_RUN != 0 && !was_running {
            if self.stuck != Some(Stuck::Run) && !self.host_system_error {
                self.halted = false;
                if self.config.specification_port_events {
                    for port in 1..=self.config.ports() {
                        if self.portsc(port) & PORT_CHANGES != 0 {
                            self.port_event(port);
                        }
                    }
                }
            }
        } else if value & CMD_RUN == 0 && was_running && self.stuck != Some(Stuck::Halt) {
            self.halted = true;
        }
    }

    fn write_port(&mut self, port: u8, value: u32) {
        let old = self.portsc(port);
        let mut new = old & !(value & PORT_CHANGES);
        if value & PORT_ENABLED != 0 {
            self.violations.push("PED written as one");
            new &= !PORT_ENABLED;
        }
        if value & PORT_RESET != 0 {
            self.violations.push("port reset started");
        }
        if !self.config.port_power_control && value & PORT_POWER == 0 {
            self.violations.push("PP written as zero");
        }
        self.ports[usize::from(port) - 1] = new;
        if self.config.port_power_control && value & PORT_POWER != old & PORT_POWER {
            let index = usize::from(port) - 1;
            if value & PORT_POWER == 0 {
                self.ports[index] = new & !(PORT_POWER | PORT_CONNECTED | PORT_ENABLED | 0xf << 10);
            } else {
                self.ports[index] = new | PORT_POWER;
                if let Some(speed) = self.devices[index] {
                    let connection = self.connection(port, speed);
                    self.change_port(port, |portsc| (portsc & !(0xf << 10)) | connection);
                }
            }
        }
    }

    fn write_register(&mut self, offset: usize, value: u64) {
        if self.gone {
            return;
        }
        let port_set = OP + 0x400;
        let port_end = port_set + 0x10 * self.ports.len();
        match offset {
            o if o == OP + USBCMD => self.write_command(value as u32),
            o if o == OP + USBSTS => {
                let clear = value as u32 & STS_RW1C;
                if clear & STS_HOST_SYSTEM_ERROR != 0 {
                    self.host_system_error = false;
                }
                if clear & STS_EVENT_INTERRUPT != 0 {
                    self.event_interrupt = false;
                }
            }
            o if o == OP + CONFIG => self.config_reg = value as u32 & 0xff,
            o if o == OP + DCBAAP => self.dcbaap = value & !0x3f,
            o if o == OP + CRCR => {
                self.crcr = value;
                self.command = value & !0x3f;
                self.command_cycle = value & 1 != 0;
            }
            o if (port_set..port_end).contains(&o) && (o - port_set).is_multiple_of(0x10) => {
                self.write_port(((o - port_set) / 0x10 + 1) as u8, value as u32)
            }
            o if o == RT + 0x20 + IMAN => {
                let value = value as u32;
                self.iman =
                    (self.iman & !(value & IMAN_PENDING) & !IMAN_ENABLE) | (value & IMAN_ENABLE);
            }
            o if o == RT + 0x20 + ERSTSZ => self.erstsz = value as u32 & 0xffff,
            o if o == RT + 0x20 + ERSTBA => self.arm_event_ring(value),
            o if o == RT + 0x20 + ERDP => {
                let busy = self.erdp & ERDP_BUSY & !(value & ERDP_BUSY);
                self.erdp = (value & !0xf) | busy;
                let pending = (self.erdp & !0xf) != self.segment + u64::from(self.enqueue) * 16;
                if busy == 0 && pending && self.segment_trbs != 0 {
                    self.raise_interrupt();
                }
            }
            o if o == DB => self.ring_command_doorbell(),
            _ => {}
        }
    }

    fn arm_event_ring(&mut self, table: u64) {
        self.erstba = table;
        if !self.bus_master {
            self.violations.push("DMA with bus mastering off");
            self.controller_error = true;
            self.halted = true;
            return;
        }
        let base = self.mem.read64(table);
        let trbs = self.mem.read32(table + 8);
        if self.erstsz != 1 || !(16..=4096).contains(&trbs) || base & 0x3f != 0 {
            self.controller_error = true;
            self.halted = true;
            return;
        }
        self.segment = base;
        self.segment_trbs = trbs as u16;
        self.enqueue = 0;
        self.event_cycle = true;
    }

    fn post_event(&mut self, trb: Trb) {
        if !self.bus_master {
            self.violations.push("DMA with bus mastering off");
            return;
        }
        if self.segment_trbs == 0 || self.halted {
            self.dropped_events += 1;
            return;
        }
        let next = (self.enqueue + 1) % self.segment_trbs;
        let dequeue = (self.erdp & !0xf)
            .checked_sub(self.segment)
            .map(|offset| offset / 16)
            .filter(|&index| index < u64::from(self.segment_trbs));
        let Some(dequeue) = dequeue else {
            self.controller_error = true;
            self.halted = true;
            return;
        };
        if u64::from(next) == dequeue {
            self.dropped_events += 1;
            return;
        }
        let at = self.segment + u64::from(self.enqueue) * 16;
        let trb = trb.with_cycle(self.event_cycle);
        self.mem.write64(at, trb.parameter);
        self.mem.write32(at + 8, trb.status);
        self.mem.write32(at + 12, trb.control);
        self.enqueue = next;
        if next == 0 {
            self.event_cycle = !self.event_cycle;
        }
        self.raise_interrupt();
    }

    fn raise_interrupt(&mut self) {
        self.event_interrupt = true;
        self.iman |= IMAN_PENDING;
        let enabled = self.iman & IMAN_ENABLE != 0 && self.usbcmd & CMD_INTE != 0;
        if enabled && self.erdp & ERDP_BUSY == 0 {
            self.interrupts += 1;
            self.erdp |= ERDP_BUSY;
        }
    }

    fn ring_command_doorbell(&mut self) {
        if self.halted || self.stuck == Some(Stuck::Commands) {
            return;
        }
        loop {
            let control = self.mem.read32(self.command + 12);
            if (control & 1 != 0) != self.command_cycle {
                return;
            }
            let trb = Trb {
                parameter: self.mem.read64(self.command),
                status: self.mem.read32(self.command + 8),
                control,
            };
            if trb.kind() == kind::LINK {
                self.command = trb.parameter & !0xf;
                if control & 1 << 1 != 0 {
                    self.command_cycle = !self.command_cycle;
                }
                continue;
            }
            let code = if trb.kind() == kind::NO_OP_COMMAND {
                1
            } else {
                5
            };
            let completion = Trb {
                parameter: self.command,
                status: code << 24,
                control: u32::from(kind::COMMAND_COMPLETION_EVENT) << 10,
            };
            self.command += 16;
            self.post_event(completion);
        }
    }

    fn port_event(&mut self, port: u8) {
        self.post_event(Trb {
            parameter: u64::from(port) << 24,
            status: 1 << 24,
            control: u32::from(kind::PORT_STATUS_CHANGE_EVENT) << 10,
        });
    }

    fn change_port(&mut self, port: u8, update: impl FnOnce(u32) -> u32) {
        let old = self.portsc(port);
        let new = update(old);
        self.ports[usize::from(port) - 1] = new;
        let raised = new & PORT_CHANGES & !old != 0;
        let blocked = self.config.specification_port_events && old & PORT_CHANGES != 0;
        if raised && !blocked && self.running() {
            self.port_event(port);
        }
    }

    /// Plug a device into `port` at Protocol Speed ID `speed`.
    pub fn attach(&mut self, port: u8, speed: u8) {
        self.devices[usize::from(port) - 1] = Some(speed);
        if self.portsc(port) & PORT_POWER == 0 {
            return;
        }
        let connection = self.connection(port, speed);
        self.change_port(port, |old| (old & !(0xf << 10)) | connection);
    }

    pub fn detach(&mut self, port: u8) {
        self.devices[usize::from(port) - 1] = None;
        self.change_port(port, |old| {
            (old & !(PORT_CONNECTED | PORT_ENABLED | 0xf << 10)) | PORT_CONNECT_CHANGE
        });
    }

    /// Post `count` events no driver acts on: MFINDEX Wrap events.
    pub fn post_idle_events(&mut self, count: usize) {
        for _ in 0..count {
            self.post_event(Trb {
                parameter: 0,
                status: 1 << 24,
                control: 39 << 10,
            });
        }
    }

    /// A PCI error the controller stops on.
    pub fn host_system_error(&mut self) {
        self.host_system_error = true;
        self.halted = true;
        self.usbcmd &= !CMD_RUN;
    }
}

impl RegisterBus for SimController {
    fn read32(&mut self, offset: usize) -> u32 {
        self.check_access();
        if !offset.is_multiple_of(4) {
            self.violations.push("misaligned register read");
        }
        let value = self.read_register(offset);
        self.log.push(Op::Read(offset, value));
        if let Some((port, device)) = self.race
            && offset == OP + 0x400 + 0x10 * usize::from(port - 1)
        {
            self.race = None;
            match device {
                Some(speed) => self.attach(port, speed),
                None => self.detach(port),
            }
        }
        value
    }

    fn write8(&mut self, offset: usize, value: u8) {
        self.check_access();
        self.log.push(Op::Write8(offset, value));
        if self.gone || self.config.legacy.is_none() {
            return;
        }
        match offset {
            o if o == XECP + 2 && value & 1 == 0 => self.bios_owned = false,
            o if o == XECP + 3 && value & 1 != 0 => {
                self.os_owned = true;
                if let Some(Legacy::ReleasesAfter(us)) = self.config.legacy
                    && self.bios_release_at.is_none()
                {
                    self.bios_release_at = Some(self.now_us + us);
                }
            }
            _ => {}
        }
    }

    fn write32(&mut self, offset: usize, value: u32) {
        self.check_access();
        if !offset.is_multiple_of(4) {
            self.violations.push("misaligned register write");
        }
        self.log.push(Op::Write32(offset, value));
        let interrupter = RT + 0x20;
        let qword = [
            OP + CRCR,
            OP + DCBAAP,
            interrupter + ERSTBA,
            interrupter + ERDP,
        ];
        if qword.iter().any(|&q| offset == q || offset == q + 4) {
            self.violations.push("64-bit register written in halves");
        }
        if offset == XECP + 4 && self.config.legacy.is_some() {
            let events = self.legacy_control & 0xe000_0000 & !value;
            self.legacy_control = (value & 0x1fff_ffff) | events;
            return;
        }
        self.write_register(offset, value.into());
        if let Some(port) = self.flapping
            && offset == OP + 0x400 + 0x10 * usize::from(port - 1)
        {
            if self.devices[usize::from(port - 1)].is_some() {
                self.detach(port);
            } else {
                self.attach(port, 3);
            }
        }
    }

    fn write64(&mut self, offset: usize, value: u64) {
        self.check_access();
        if !offset.is_multiple_of(8) {
            self.violations.push("misaligned register write");
        }
        self.log.push(Op::Write64(offset, value));
        self.write_register(offset, value);
    }

    fn bus_master(&mut self, on: bool) {
        self.log.push(Op::BusMaster(on));
        self.bus_master = on;
    }

    fn delay_us(&mut self, us: u32) {
        self.log.push(Op::Delay(us));
        self.now_us += u64::from(us);
    }
}

mod tests;
