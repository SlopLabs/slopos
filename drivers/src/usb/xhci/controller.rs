//! One running controller: its rings behind their locks, the drain the
//! interrupt handler and the USB thread share, its root ports, and what
//! happens when it dies or the machine goes down.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use slopos_mm::mmio::MmioRegion;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, Mutex, MutexGuard, OnceLock, SpinLock};
use slopos_ostd::{KArc, KVec, klog_info, lock_class, write_field};
use slopos_usb_core::xhci::controller::{self as seq, Handoff, Health};
use slopos_usb_core::xhci::memory::{list_scratchpads, write_segment_table};
use slopos_usb_core::xhci::ring::{
    CommandCompletion, CommandResult, RING_TRBS, SubmitError, Ticket,
};
use slopos_usb_core::xhci::{
    Capabilities, CommandRing, CompletionCode, DmaPage, Event, EventRing, Layout, Protocols, Setup,
    Trb,
};

use super::page::{Bus, Page};
use crate::driver_core::shutdown::DeviceShutdown;
use crate::pci_defs::PciDeviceInfo;

const STARTING: u8 = 0;
const RUNNING: u8 = 1;
const DEAD: u8 = 2;
const STOPPED: u8 = 3;

/// A bit for every port number a `u8` holds.
const PORT_WORDS: usize = 256 / 64;

/// What a root port last showed, and how often it changed: the USB thread
/// writes it, tests read it.
#[derive(Default)]
pub struct Port {
    connected: AtomicBool,
    attaches: AtomicU32,
    detaches: AtomicU32,
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
    /// the command lock nests inside it.
    events: SpinLock<EventRing<Page>>,
    commands: SpinLock<CommandRing<Page>>,
    _tables: KVec<Page>,
    ports: KVec<Port>,
    /// Root ports a Port Status Change Event named, by number, for the
    /// thread to look at again.
    port_changes: [AtomicU64; PORT_WORDS],
    state: AtomicU8,
    /// What a drain found wrong, for the thread to act on.
    failure: AtomicU8,
    ring_full: AtomicBool,
    interrupts: AtomicU32,
    /// Serialises the thread's work on the controller with its shutdown.
    service: Mutex<()>,
    setup: Setup,
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
                    ports.push(Port::default())?;
                }
                Self::install_memory(&slot, &caps)?;
                write_field!(slot, number, number);
                write_field!(slot, info, *info);
                write_field!(slot, regs, regs);
                write_field!(slot, caps, caps);
                write_field!(slot, layout, caps.layout());
                write_field!(slot, protocols, *protocols);
                write_field!(slot, ports, ports);
                write_field!(
                    slot,
                    port_changes,
                    [const { AtomicU64::new(0) }; PORT_WORDS]
                );
                write_field!(slot, state, AtomicU8::new(STARTING));
                write_field!(slot, failure, AtomicU8::new(Health::Running as u8));
                write_field!(slot, ring_full, AtomicBool::new(false));
                write_field!(slot, interrupts, AtomicU32::new(0));
                write_field!(
                    slot,
                    service,
                    Mutex::new((), lock_class!("Xhci.service", LOCK_LEVEL_RESOURCE))
                );
                write_field!(slot, irq, OnceLock::new());
                Ok(slot.finish())
            },
        ))
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
            Setup {
                slots: caps.max_slots,
                dcbaa: dcbaa.phys(),
                crcr: commands.crcr(),
                segment_table: segment_table.phys(),
                event_ring: events.dequeue_pointer(),
            }
        );
        tables.push(dcbaa)?;
        tables.push(segment_table)?;
        write_field!(slot, _tables, tables);
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

    fn bus(&self) -> Bus<'_> {
        Bus {
            regs: &self.regs,
            info: &self.info,
        }
    }

    pub fn number(&self) -> u8 {
        self.number
    }

    #[cfg(feature = "test-hooks")]
    pub fn ids(&self) -> (u16, u16) {
        (self.info.vendor_id, self.info.device_id)
    }

    pub fn interrupt(&self) -> &'static str {
        self.irq.get().copied().unwrap_or("no interrupt")
    }

    #[cfg(feature = "test-hooks")]
    pub fn max_ports(&self) -> u8 {
        self.caps.max_ports
    }

    pub fn is_running(&self) -> bool {
        self.state.load(Ordering::Acquire) == RUNNING
    }

    fn port(&self, port: u8) -> Option<&Port> {
        self.ports.get(usize::from(port).checked_sub(1)?)
    }

    #[cfg(feature = "test-hooks")]
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

    /// Hand every event the ring holds to whoever waits on it; whether any
    /// work is left for the thread. Bounded and allocation-free, as it runs
    /// in the interrupt handler.
    pub(super) fn drain(&self) -> bool {
        if !self.is_running() {
            return false;
        }
        let mut bus = self.bus();
        let mut work = false;
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
                    work |= self.take_event(event);
                },
            )
        };
        if drained.health != Health::Running {
            self.failure.store(drained.health as u8, Ordering::Release);
            work = true;
        }
        work
    }

    /// Record one event; whether it left the thread something to do.
    fn take_event(&self, event: Event) -> bool {
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
                false
            }
            Event::PortStatusChange { port } if (1..=self.caps.max_ports).contains(&port) => {
                self.flag_port(port);
                true
            }
            Event::HostController {
                code: CompletionCode::EVENT_RING_FULL,
            } => {
                self.ring_full.store(true, Ordering::Release);
                true
            }
            _ => false,
        }
    }

    fn flag_port(&self, port: u8) {
        self.port_changes[usize::from(port / 64)].fetch_or(1 << (port % 64), Ordering::AcqRel);
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

    /// A killed task still gets the lock: nothing under it sleeps, so its
    /// acquire spins.
    fn service_lock(&self) -> MutexGuard<'_, ()> {
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

    /// The USB thread's turn: notice a death, drain what a lost interrupt
    /// left in the ring, and look again at every root port an event named.
    pub(super) fn serve(&self) {
        let _service = self.service_lock();
        if !self.is_running() {
            return;
        }
        self.drain();
        let health = Health::from_u8(self.failure.swap(Health::Running as u8, Ordering::AcqRel));
        if health != Health::Running {
            self.die(health);
            return;
        }
        for (word, changes) in self.port_changes.iter().enumerate() {
            let mut pending = changes.swap(0, Ordering::AcqRel);
            while pending != 0 {
                let bit = pending.trailing_zeros();
                pending &= pending - 1;
                self.check_port((word as u32 * 64 + bit) as u8);
            }
        }
        if self.ring_full.swap(false, Ordering::AcqRel) {
            klog_info!(
                "USB: xhci {} lost events: its event ring filled",
                self.number
            );
        }
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
            self.check_port(port);
        }
    }

    fn check_port(&self, port: u8) {
        let Some(state) = self.port(port) else {
            return;
        };
        let Some(read) = seq::acknowledge_port(&mut self.bus(), &self.layout, port) else {
            return;
        };
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

    /// Halt a dead controller, or find it halted, and take it off the bus.
    /// It is not reset, and not probed again this boot.
    fn die(&self, health: Health) {
        if !self.leave(Some(RUNNING), DEAD) {
            return;
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
            cause(health),
            halted
        );
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
    /// Halt and reset the controller, dead or alive, and take it off the
    /// bus, so the next firmware finds it as a reset leaves it.
    fn shutdown(&self) {
        let _service = self.service_lock();
        if self.leave(Some(RUNNING), STOPPED) || self.leave(Some(DEAD), STOPPED) {
            self.reset_off_the_bus(seq::SHUTDOWN_RESET_MS);
        }
    }
}
