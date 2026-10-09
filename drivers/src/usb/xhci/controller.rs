//! One running controller: its rings, the drain the interrupt handler and
//! the USB thread share, its root ports and tree, its death and shutdown.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use slopos_mm::mmio::MmioRegion;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, Mutex, MutexGuard, OnceLock, SpinLock};
use slopos_ostd::{KArc, KVec, klog_info, lock_class, write_field};
use slopos_usb_core::bus::{Hub, Node, Port as TreePort, State as TreeState, Tree};
use slopos_usb_core::device::Speed;
use slopos_usb_core::hub::PortStatus;
use slopos_usb_core::xhci::context::ContextLayout;
use slopos_usb_core::xhci::controller::{self as seq, Handoff, Health};
use slopos_usb_core::xhci::memory::{list_scratchpads, set_device_context, write_segment_table};
use slopos_usb_core::xhci::regs::PortSc;
use slopos_usb_core::xhci::ring::{
    CommandCompletion, CommandResult, RING_TRBS, SubmitError, Ticket,
};
use slopos_usb_core::xhci::{
    Capabilities, CommandRing, CompletionCode, DmaPage, Event, EventRing, Layout, Protocols, Trb,
};

use super::device::Device;
use super::host::ControllerHost;
use super::page::{Bus, Page};
use crate::driver_core::shutdown::DeviceShutdown;
use crate::pci_defs::PciDeviceInfo;

const STARTING: u8 = 0;
const RUNNING: u8 = 1;
const DEAD: u8 = 2;
const STOPPED: u8 = 3;

/// A bit for every port number a `u8` holds.
const PORT_WORDS: usize = 256 / 64;

const MAX_HUBS: usize = 8;

/// What a root port last showed, and how often it changed: the USB thread
/// writes it, tests read it.
#[derive(Default)]
pub struct RootPort {
    connected: AtomicBool,
    attaches: AtomicU32,
    detaches: AtomicU32,
}

pub(super) struct Service {
    roots: KVec<TreePort>,
    nodes: KVec<Node>,
    hubs: KVec<Hub>,
    state: TreeState,
}

impl Service {
    fn new(ports: u8, slots: u8) -> Result<Self, AllocError> {
        let mut service = Self {
            roots: KVec::new(),
            nodes: KVec::new(),
            hubs: KVec::new(),
            state: TreeState::default(),
        };
        for _ in 0..ports {
            service.roots.push(TreePort::default())?;
        }
        for _ in 0..=slots {
            service.nodes.push(Node::default())?;
        }
        for _ in 0..MAX_HUBS {
            service.hubs.push(Hub::default())?;
        }
        Ok(service)
    }

    pub(super) fn tree(&mut self) -> Tree<'_> {
        Tree {
            roots: &mut self.roots,
            nodes: &mut self.nodes,
            hubs: &mut self.hubs,
            state: &mut self.state,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Took {
    work: bool,
    /// A driver may be waiting on it.
    transfer: bool,
    report: bool,
    sink: bool,
}

#[derive(slopos_ostd::SlotFields)]
pub struct Controller {
    number: u8,
    info: PciDeviceInfo,
    regs: MmioRegion,
    caps: Capabilities,
    layout: Layout,
    protocols: Protocols,
    /// Held by the drain, and by a state change so that none is mid-drain;
    /// the command lock and the device table nest inside it.
    events: SpinLock<EventRing<Page>>,
    commands: SpinLock<CommandRing<Page>>,
    dcbaa: SpinLock<Page>,
    _tables: KVec<Page>,
    /// The drain finds a transfer's ring here.
    devices: SpinLock<KVec<Option<KArc<Device>>>>,
    ports: KVec<RootPort>,
    /// Root ports a Port Status Change Event named, by number, for the
    /// thread to look at again.
    port_changes: [AtomicU64; PORT_WORDS],
    /// Slots with a report completed, by number.
    reports_done: [AtomicU64; PORT_WORDS],
    /// Slots with a transfer for a sink completed, by number.
    sinks_done: [AtomicU64; PORT_WORDS],
    state: AtomicU8,
    /// What a drain found wrong, for the thread to act on.
    failure: AtomicU8,
    /// A driver's command never completed.
    stuck: AtomicBool,
    ring_full: AtomicBool,
    interrupts: AtomicU32,
    /// Bumped whenever the thread is left something to do.
    work: AtomicU64,
    /// The thread's work on the controller, serialised with its shutdown.
    service: Mutex<Service>,
    /// `work` when a pass that began there found the tree settled.
    settled_at: AtomicU64,
    setup: seq::Setup,
    irq: OnceLock<&'static str>,
}

fn cause(health: Health) -> &'static str {
    match health {
        Health::Running => "running",
        Health::HostSystemError => "host system error",
        Health::ControllerError => "host controller error",
        Health::Absent => "registers read all ones",
    }
}

impl Controller {
    /// The controller with the pages it is given: the DCBAA, the
    /// scratchpads, the command ring, the event ring and its segment table.
    pub(super) fn new(
        number: u8,
        info: &PciDeviceInfo,
        regs: MmioRegion,
        caps: Capabilities,
        protocols: &Protocols,
    ) -> Result<KArc<Self>, AllocError> {
        KArc::try_init(init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                let mut ports = KVec::new();
                for _ in 0..caps.max_ports {
                    ports.push(RootPort::default())?;
                }
                let mut devices = KVec::new();
                for _ in 0..=caps.max_slots {
                    devices.push(None)?;
                }
                let service = Service::new(caps.max_ports, caps.max_slots)?;
                Self::install_memory(&slot, &caps)?;
                write_field!(slot, number, number);
                write_field!(slot, info, *info);
                write_field!(slot, regs, regs);
                write_field!(slot, caps, caps);
                write_field!(slot, layout, caps.layout());
                write_field!(slot, protocols, *protocols);
                write_field!(
                    slot,
                    devices,
                    SpinLock::new(devices, lock_class!("Xhci.devices", LOCK_LEVEL_RESOURCE))
                );
                write_field!(slot, ports, ports);
                Self::install_flags(&slot);
                write_field!(
                    slot,
                    service,
                    Mutex::new(service, lock_class!("Xhci.service", LOCK_LEVEL_RESOURCE))
                );
                write_field!(slot, settled_at, AtomicU64::new(u64::MAX));
                write_field!(slot, irq, OnceLock::new());
                Ok(slot.finish())
            },
        ))
    }

    #[inline(never)]
    fn install_flags(slot: &SlotPtr<Self>) {
        write_field!(
            slot,
            port_changes,
            [const { AtomicU64::new(0) }; PORT_WORDS]
        );
        write_field!(
            slot,
            reports_done,
            [const { AtomicU64::new(0) }; PORT_WORDS]
        );
        write_field!(slot, sinks_done, [const { AtomicU64::new(0) }; PORT_WORDS]);
        write_field!(slot, stuck, AtomicBool::new(false));
        write_field!(slot, state, AtomicU8::new(STARTING));
        write_field!(slot, failure, AtomicU8::new(Health::Running as u8));
        write_field!(slot, ring_full, AtomicBool::new(false));
        write_field!(slot, interrupts, AtomicU32::new(0));
        write_field!(slot, work, AtomicU64::new(0));
    }

    /// The pages the controller is given, and the rings over them.
    #[inline(never)]
    fn install_memory(slot: &SlotPtr<Self>, caps: &Capabilities) -> Result<(), AllocError> {
        let page = || Page::alloc().ok_or(AllocError);
        let mut tables = KVec::new();
        for _ in 0..caps.scratchpads {
            tables.push(page()?)?;
        }
        let mut dcbaa = page()?;
        if caps.scratchpads > 0 {
            let mut array = page()?;
            list_scratchpads(&mut dcbaa, &mut array, tables.iter().map(DmaPage::phys));
            tables.push(array)?;
        }
        let events = EventRing::new(page()?);
        let mut segment_table = page()?;
        write_segment_table(&mut segment_table, events.phys(), RING_TRBS);
        let commands = CommandRing::new(page()?);
        write_field!(
            slot,
            setup,
            seq::Setup {
                slots: caps.max_slots,
                dcbaa: dcbaa.phys(),
                crcr: commands.crcr(),
                segment_table: segment_table.phys(),
                event_ring: events.dequeue_pointer(),
            }
        );
        tables.push(segment_table)?;
        write_field!(slot, _tables, tables);
        write_field!(
            slot,
            dcbaa,
            SpinLock::new(dcbaa, lock_class!("Xhci.dcbaa", LOCK_LEVEL_RESOURCE))
        );
        write_field!(
            slot,
            events,
            SpinLock::new(events, lock_class!("Xhci.events", LOCK_LEVEL_RESOURCE))
        );
        write_field!(
            slot,
            commands,
            SpinLock::new(commands, lock_class!("Xhci.commands", LOCK_LEVEL_RESOURCE))
        );
        Ok(())
    }

    /// Ask the firmware for the controller through USB Legacy Support at
    /// `legacy`.
    pub(super) fn take_ownership(&self, legacy: Option<usize>) -> Handoff {
        seq::take_ownership(&mut self.bus(), legacy)
    }

    /// Halt the controller, take it off the bus and reset it; one that will
    /// not halt is left off the bus unreset.
    pub(super) fn reset(&self) -> Result<(), slopos_usb_core::xhci::Error> {
        let mut bus = self.bus();
        let reset = seq::halt_and_reset(&mut bus, &self.layout, seq::PROBE_RESET_MS);
        if reset.is_err() {
            slopos_usb_core::xhci::RegisterBus::bus_master(&mut bus, false);
        }
        reset
    }

    /// Let the reset controller master the bus and give it its memory.
    pub(super) fn configure(&self) {
        seq::configure(&mut self.bus(), &self.layout, &self.setup);
    }

    pub(super) fn run(&self) -> Result<(), slopos_usb_core::xhci::Error> {
        seq::start(&mut self.bus(), &self.layout)?;
        self.state.store(RUNNING, Ordering::Release);
        Ok(())
    }

    pub(super) fn set_interrupt(&self, kind: &'static str) {
        self.irq.call_once(|| kind);
    }

    pub(super) fn bus(&self) -> Bus<'_> {
        Bus {
            regs: &self.regs,
            info: &self.info,
        }
    }

    pub fn number(&self) -> u8 {
        self.number
    }

    pub fn ids(&self) -> (u16, u16) {
        (self.info.vendor_id, self.info.device_id)
    }

    pub fn bdf(&self) -> (u8, u8, u8) {
        (self.info.bus, self.info.device, self.info.function)
    }

    pub fn interrupt(&self) -> &'static str {
        self.irq.get().copied().unwrap_or("no interrupt")
    }

    pub fn max_ports(&self) -> u8 {
        self.caps.max_ports
    }

    pub fn is_running(&self) -> bool {
        self.state.load(Ordering::Acquire) == RUNNING
    }

    pub fn state_name(&self) -> &'static str {
        match self.state.load(Ordering::Acquire) {
            STARTING => "starting",
            RUNNING => "running",
            DEAD => "dead",
            _ => "stopped",
        }
    }

    pub(super) fn protocols(&self) -> &Protocols {
        &self.protocols
    }

    pub(super) fn contexts(&self) -> ContextLayout {
        ContextLayout::new(self.caps.context_64)
    }

    fn port(&self, port: u8) -> Option<&RootPort> {
        self.ports.get(usize::from(port).checked_sub(1)?)
    }

    pub fn port_connected(&self, port: u8) -> bool {
        self.port(port)
            .is_some_and(|p| p.connected.load(Ordering::Acquire))
    }

    #[cfg(feature = "test-hooks")]
    pub fn port_attaches(&self, port: u8) -> u32 {
        self.port(port)
            .map_or(0, |p| p.attaches.load(Ordering::Acquire))
    }

    #[cfg(feature = "test-hooks")]
    pub fn port_detaches(&self, port: u8) -> u32 {
        self.port(port)
            .map_or(0, |p| p.detaches.load(Ordering::Acquire))
    }

    pub(super) fn handle_irq(&self) {
        self.interrupts.fetch_add(1, Ordering::Relaxed);
        if self.drain() {
            crate::usb::wake();
        }
    }

    /// Interrupts taken since probe.
    #[cfg(feature = "test-hooks")]
    pub fn interrupts_taken(&self) -> u32 {
        self.interrupts.load(Ordering::Relaxed)
    }

    /// Whether work is left for the thread. Bounded and allocation-free, as
    /// the interrupt handler runs it; waiting drivers are woken once the event
    /// lock is released.
    pub(super) fn note_work(&self) {
        self.work.fetch_add(1, Ordering::AcqRel);
    }

    pub(super) fn drain(&self) -> bool {
        if !self.is_running() {
            return false;
        }
        let mut bus = self.bus();
        let mut took = Took::default();
        let drained = {
            let mut events = self.events.lock();
            if !self.is_running() {
                return false;
            }
            seq::drain(
                &mut bus,
                &self.layout,
                &mut events,
                seq::DRAIN_BUDGET,
                |event| {
                    let one = self.take_event(event);
                    took.work |= one.work;
                    took.transfer |= one.transfer;
                    took.report |= one.report;
                    took.sink |= one.sink;
                },
            )
        };
        if drained.health != Health::Running {
            self.failure.store(drained.health as u8, Ordering::Release);
            took.work = true;
        }
        if took.work {
            self.note_work();
        }
        if took.sink {
            self.dispatch_sinks();
        }
        if took.transfer {
            crate::usb::TRANSFERS.wake_all();
        }
        if took.report {
            self.dispatch_reports();
        }
        took.work
    }

    fn take_event(&self, event: Event) -> Took {
        match event {
            Event::CommandCompletion {
                trb,
                code,
                parameter,
                slot,
            } => {
                let completion = CommandCompletion {
                    code,
                    parameter,
                    slot,
                };
                self.commands.lock().complete(trb, completion);
                Took {
                    work: true,
                    transfer: true,
                    ..Took::default()
                }
            }
            Event::Transfer {
                trb,
                residual,
                code,
                dci,
                slot,
                ..
            } => {
                let devices = self.devices.lock();
                let finished = devices
                    .get(usize::from(slot))
                    .and_then(Option::as_ref)
                    .map(|d| d.complete(dci, trb, code, residual))
                    .unwrap_or_default();
                if finished.report {
                    Self::flag(&self.reports_done, slot);
                }
                if finished.sink {
                    Self::flag(&self.sinks_done, slot);
                }
                Took {
                    work: finished.transfer && finished.tree,
                    transfer: finished.transfer && !finished.report && !finished.sink,
                    report: finished.report,
                    sink: finished.sink,
                }
            }
            Event::PortStatusChange { port } if (1..=self.caps.max_ports).contains(&port) => {
                self.flag_port(port);
                Took {
                    work: true,
                    ..Took::default()
                }
            }
            Event::HostController {
                code: CompletionCode::EVENT_RING_FULL,
            } => {
                self.ring_full.store(true, Ordering::Release);
                Took {
                    work: true,
                    ..Took::default()
                }
            }
            _ => Took::default(),
        }
    }

    fn flag(bits: &[AtomicU64; PORT_WORDS], n: u8) {
        bits[usize::from(n / 64)].fetch_or(1 << (n % 64), Ordering::AcqRel);
    }

    fn flag_port(&self, port: u8) {
        Self::flag(&self.port_changes, port);
    }

    /// The lowest root port an event named, then forgotten.
    pub(super) fn take_port_change(&self) -> Option<u8> {
        Self::take_lowest(&self.port_changes)
    }

    /// Each device's sinks hear of their completions under the device
    /// table's lock, which keeps the device from being freed under them.
    fn dispatch_sinks(&self) {
        while let Some(slot) = Self::take_lowest(&self.sinks_done) {
            let devices = self.devices.lock();
            if let Some(device) = devices.get(usize::from(slot)).and_then(Option::as_ref) {
                device.dispatch_sinks();
            }
        }
    }

    /// A driver's command never completed: the thread takes the controller
    /// for dead, as it does when one of the tree's does not.
    pub(super) fn note_stuck(&self) {
        self.stuck.store(true, Ordering::Release);
        self.note_work();
        crate::usb::wake();
    }

    /// Each device's reports go to its sinks under the device table's lock,
    /// which keeps the device from being freed under them.
    fn dispatch_reports(&self) {
        while let Some(slot) = Self::take_lowest(&self.reports_done) {
            let devices = self.devices.lock();
            if let Some(device) = devices.get(usize::from(slot)).and_then(Option::as_ref) {
                device.dispatch_reports();
            }
        }
    }

    fn take_lowest(bits: &[AtomicU64; PORT_WORDS]) -> Option<u8> {
        for (word, changes) in bits.iter().enumerate() {
            let mut pending = changes.load(Ordering::Acquire);
            while pending != 0 {
                let bit = pending.trailing_zeros();
                match changes.compare_exchange(
                    pending,
                    pending & !(1 << bit),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some((word as u32 * 64 + bit) as u8),
                    Err(seen) => pending = seen,
                }
            }
        }
        None
    }

    pub(super) fn submit(&self, trb: Trb) -> Result<Ticket, SubmitError> {
        let mut bus = self.bus();
        let doorbell = seq::command_doorbell(&self.layout);
        self.commands.lock().submit(&mut bus, doorbell, trb)
    }

    pub(super) fn take(&self, ticket: Ticket) -> Option<CommandResult> {
        self.commands.lock().take(ticket)
    }

    pub(super) fn abandon(&self, ticket: Ticket) {
        self.commands.lock().abandon(ticket);
    }

    pub(super) fn device(&self, slot: u8) -> Option<KArc<Device>> {
        self.devices
            .lock()
            .get(usize::from(slot))
            .and_then(Option::as_ref)
            .map(KArc::clone)
    }

    /// None if the list cannot be allocated.
    pub fn devices(&self) -> KVec<KArc<Device>> {
        let Ok(mut out) = KVec::with_capacity(usize::from(self.caps.max_slots) + 1) else {
            return KVec::new();
        };
        for device in self.devices.lock().iter().flatten() {
            if out.push(KArc::clone(device)).is_err() {
                break;
            }
        }
        out
    }

    pub(super) fn create_device(&self, slot: u8, serial: u64) -> bool {
        let Ok(device) = Device::new(
            self.number,
            slot,
            serial,
            self.regs.clone(),
            self.layout.doorbell(slot),
            self.contexts(),
        ) else {
            return false;
        };
        let output = device.output_phys();
        match self.devices.lock().get_mut(usize::from(slot)) {
            Some(entry) => *entry = Some(device),
            None => return false,
        }
        set_device_context(&mut *self.dcbaa.lock(), slot, output);
        true
    }

    /// The DCBAA entry is cleared first, then the drivers' claims go.
    pub(super) fn destroy_device(&self, slot: u8) {
        set_device_context(&mut *self.dcbaa.lock(), slot, 0);
        let device = self
            .devices
            .lock()
            .get_mut(usize::from(slot))
            .and_then(Option::take);
        if let Some(device) = device {
            crate::usb::bus::release_claims(&device);
        }
    }

    #[cfg(feature = "test-hooks")]
    pub fn slots_in_use(&self) -> usize {
        self.devices.lock().iter().flatten().count()
    }

    /// A killed task spins for it: the shutdown hook must have it, and the USB
    /// thread holds it only for a pass.
    pub(super) fn service_lock(&self) -> MutexGuard<'_, Service> {
        match self.service.lock() {
            Ok(guard) => guard,
            Err(_) => loop {
                if let Some(guard) = self.service.try_lock() {
                    break guard;
                }
                core::hint::spin_loop();
            },
        }
    }

    /// Notices a death, drains what a lost interrupt left and steps the tree;
    /// returns when it next needs a pass.
    pub(super) fn serve(&self) -> Option<u64> {
        let mut service = self.service_lock();
        match self.state.load(Ordering::Acquire) {
            RUNNING => {
                self.drain();
                let health =
                    Health::from_u8(self.failure.swap(Health::Running as u8, Ordering::AcqRel));
                if health != Health::Running {
                    self.die(cause(health), &mut service);
                } else if self.stuck.swap(false, Ordering::AcqRel) {
                    self.die("a command never completed", &mut service);
                }
            }
            DEAD => {}
            _ => return None,
        }
        if self.ring_full.swap(false, Ordering::AcqRel) {
            klog_info!(
                "USB: xhci {} lost events: its event ring filled",
                self.number
            );
        }
        if !crate::usb::enumerating() {
            for _ in 0..self.caps.max_ports {
                let Some(port) = self.take_port_change() else {
                    break;
                };
                self.read_port(port);
            }
            return None;
        }
        let seen = self.work.load(Ordering::Acquire);
        let mut host = ControllerHost::new(self);
        let mut tree = service.tree();
        let next = tree.step(&mut host);
        let settled = tree.settled(&mut host) || (self.state() == DEAD && tree.is_empty());
        self.settled_at
            .store(if settled { seen } else { u64::MAX }, Ordering::Release);
        next
    }

    fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    /// And no port change waits for the thread; a stopped controller has.
    pub fn is_settled(&self) -> bool {
        let quiet = self
            .port_changes
            .iter()
            .all(|changes| changes.load(Ordering::Acquire) == 0);
        let settled = self.settled_at.load(Ordering::Acquire) == self.work.load(Ordering::Acquire);
        self.state() == STOPPED || (quiet && settled)
    }

    /// Power every root port the controller switches and look at each once,
    /// clearing what changed while it was halted.
    pub(super) fn scan_ports(&self) {
        let _service = self.service_lock();
        let mut bus = self.bus();
        for port in 1..=self.caps.max_ports {
            seq::power_port(&mut bus, &self.layout, port);
        }
        for port in 1..=self.caps.max_ports {
            self.read_port(port);
        }
    }

    /// Logs what arrived and left.
    pub(super) fn read_port(&self, port: u8) -> Option<PortStatus> {
        let state = self.port(port)?;
        let read = seq::acknowledge_port(&mut self.bus(), &self.layout, port)?;
        if read.unsettled {
            self.flag_port(port);
        }
        let status = read.status;
        let was = state.connected.load(Ordering::Acquire);
        let now = status.connected();
        if was && (!now || status.connect_changed()) {
            state.detaches.fetch_add(1, Ordering::AcqRel);
            klog_info!("USB: {}-{} detached", self.number, port);
        }
        if now && (!was || status.connect_changed()) {
            state.attaches.fetch_add(1, Ordering::AcqRel);
            self.log_attach(port, status.speed());
        }
        state.connected.store(now, Ordering::Release);
        if status.over_current_changed() {
            let over = if status.over_current() {
                "over"
            } else {
                "back under"
            };
            klog_info!("USB: {}-{} {} current", self.number, port, over);
        }
        let speed = self
            .protocols
            .speed(port, status.speed())
            .map(|s| Speed::from_bits_per_second(s.bits_per_second));
        Some(status.status(speed))
    }

    pub(super) fn write_port(&self, port: u8, write: impl FnOnce(PortSc) -> u32) {
        let at = self.layout.port(port);
        let portsc = PortSc(self.regs.read::<u32>(at));
        if portsc.0 != u32::MAX {
            self.regs.write::<u32>(at, write(portsc));
        }
    }

    #[inline(never)]
    fn log_attach(&self, port: u8, psiv: u8) {
        let Some(protocol) = self.protocols.of_port(port) else {
            klog_info!(
                "USB: {}-{} attached, speed {}, on a port no protocol names",
                self.number,
                port,
                psiv
            );
            return;
        };
        let (major, minor) = protocol.revision();
        let number = self.number;
        match self.protocols.speed(port, psiv) {
            Some(speed) => match speed.name {
                Some(name) => klog_info!(
                    "USB: {}-{} attached, {} (USB {}.{} port)",
                    number,
                    port,
                    name,
                    major,
                    minor
                ),
                None => klog_info!(
                    "USB: {}-{} attached, {} Mb/s (USB {}.{} port)",
                    number,
                    port,
                    speed.bits_per_second / 1_000_000,
                    major,
                    minor
                ),
            },
            None => klog_info!(
                "USB: {}-{} attached, unknown speed {} (USB {}.{} port)",
                number,
                port,
                psiv,
                major,
                minor
            ),
        }
    }

    /// Move the state the IRQ path reads from `from` to `to` behind the
    /// event lock, so no drain is still running once this returns; any
    /// state when `from` is `None`.
    fn leave(&self, from: Option<u8>, to: u8) -> bool {
        let _events = self.events.lock();
        let state = self.state.load(Ordering::Acquire);
        if from.is_some_and(|from| from != state) {
            return false;
        }
        self.state.store(to, Ordering::Release);
        true
    }

    /// Every device goes, with no command issued.
    fn die(&self, cause: &str, service: &mut Service) {
        if self.kill(cause) {
            service.tree().die(&mut ControllerHost::new(self));
            crate::usb::TRANSFERS.wake_all();
        }
    }

    /// Halts the controller, takes it off the bus and fails its commands;
    /// whether it was running. It is not reset or probed again.
    pub(super) fn kill(&self, cause: &str) -> bool {
        if !self.leave(Some(RUNNING), DEAD) {
            return false;
        }
        let halted = seq::quiesce(&mut self.bus(), &self.layout);
        self.commands.lock().fail_all();
        let halted = if halted.is_ok() {
            "halted"
        } else {
            "not halted"
        };
        klog_info!(
            "USB: xhci {} died: {}; {}, bus mastering off",
            self.number,
            cause,
            halted
        );
        true
    }

    /// Whether the controller is halted, reset and off the bus, as a
    /// shutdown leaves it: a reset clears DCBAAP, which a halt keeps.
    #[cfg(feature = "test-hooks")]
    pub fn is_reset(&self) -> bool {
        use slopos_usb_core::xhci::regs::{CMD_RESET, CMD_RUN, DCBAAP, STS_HALTED, USBCMD, USBSTS};
        let cmd = self.regs.read::<u32>(self.layout.op(USBCMD));
        let sts = self.regs.read::<u32>(self.layout.op(USBSTS));
        let dcbaap = self.regs.read::<u32>(self.layout.op(DCBAAP))
            | self.regs.read::<u32>(self.layout.op(DCBAAP) + 4);
        let command = crate::pci::pci_config_read16(
            self.info.bus,
            self.info.device,
            self.info.function,
            crate::pci_defs::PCI_COMMAND_OFFSET,
        );
        cmd & (CMD_RUN | CMD_RESET) == 0
            && sts & STS_HALTED != 0
            && dcbaap == 0
            && command & crate::pci_defs::PCI_COMMAND_BUS_MASTER == 0
    }

    /// Halt and reset the controller and take it off the bus, whatever state
    /// it is in, waiting as long as probe does; a probe that fails once the
    /// controller has its memory ends here.
    pub(super) fn stop(&self) {
        self.leave(None, STOPPED);
        self.reset_off_the_bus(seq::PROBE_RESET_MS);
    }

    fn reset_off_the_bus(&self, bound_ms: u32) {
        let mut bus = self.bus();
        let reset = seq::halt_and_reset(&mut bus, &self.layout, bound_ms);
        slopos_usb_core::xhci::RegisterBus::bus_master(&mut bus, false);
        self.commands.lock().fail_all();
        match reset {
            Ok(()) => klog_info!("USB: xhci {} reset, off the bus", self.number),
            Err(err) => klog_info!(
                "USB: xhci {} not reset: {:?}; bus mastering off",
                self.number,
                err
            ),
        }
    }
}

impl DeviceShutdown for Controller {
    /// Drain and flush its sticks, then halt and reset the controller, dead or
    /// alive, and take it off the bus, so the next firmware finds it as a
    /// reset leaves it.
    fn shutdown(&self) {
        let _service = self.service_lock();
        if self.is_running() {
            crate::usb::storage::shutdown(self.number, &|| {
                self.drain();
            });
        }
        if self.leave(Some(RUNNING), STOPPED) || self.leave(Some(DEAD), STOPPED) {
            self.reset_off_the_bus(seq::SHUTDOWN_RESET_MS);
        }
    }
}
