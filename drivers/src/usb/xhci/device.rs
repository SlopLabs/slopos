//! One device in a slot: its contexts, rings and buffers behind the lock the
//! drain and the threads share, its descriptors, its bindings, and the
//! blocking transfers a bound driver makes.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use slopos_mm::mmio::MmioRegion;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, Mutex, MutexGuard, SpinLock, WaitAbort};
use slopos_ostd::{KArc, KBox, KVec, klog_info, lock_class, write_field};
use slopos_usb_core::bus::{
    COMMAND_MS, MAX_ENDPOINTS, Node, STORE_CONFIGURATION, alternate_change,
    for_each_configured_endpoint,
};
use slopos_usb_core::device::Speed;
use slopos_usb_core::device::descriptor::{self, Configuration, Function};
use slopos_usb_core::device::request::Setup;
use slopos_usb_core::xhci::context::{
    ContextLayout, EndpointContext, InputControlContext, MAX_READDED, SlotContext, endpoint_state,
    read_context, write_input, write_readded,
};
use slopos_usb_core::xhci::memory::PAGE_SIZE;
use slopos_usb_core::xhci::ring::{CommandResult, SubmitError, Ticket};
use slopos_usb_core::xhci::transfer::{
    MAX_TD_PAGES, PushError, Transfer, TransferError, TransferResult,
};
use slopos_usb_core::xhci::{CompletionCode, DmaPage, TransferRing, Trb};

use super::page::{Page, Store};
use crate::driver_core::bound::BoundError;
use crate::driver_core::bus::ClaimSlot;

/// USB 2.0 §9.2.6.4.
pub const CONTROL_MS: u64 = 5000;
/// Halts a reporting endpoint or a [`Queue`] is recovered from, each within
/// [`HALT_WINDOW_MS`] of the last and with nothing received between.
const HALT_RECOVERIES: u8 = 3;
const HALT_WINDOW_MS: u64 = 1000;
/// Interfaces numbered past this stay in alternate setting 0.
pub const ALTERNATES: usize = 32;

struct Endpoint {
    dci: u8,
    descriptor: descriptor::Endpoint,
    ring: TransferRing<Page>,
    /// Given out with the pipe, or the tree's first transfer.
    buffer: Option<Page>,
    open: bool,
    /// Bytes of each report a [`Reports`] keeps posted, 0 for none.
    report_length: u32,
    posted: Option<Transfer>,
    /// Recoveries since a report last arrived, and when it last halted.
    recoveries: u8,
    halted_at: u64,
}

impl Endpoint {
    #[inline(never)]
    fn new(dci: u8, descriptor: descriptor::Endpoint) -> Result<Self, AllocError> {
        Ok(Self {
            dci,
            descriptor,
            ring: TransferRing::new(Page::alloc().ok_or(AllocError)?),
            buffer: None,
            open: false,
            report_length: 0,
            posted: None,
            recoveries: 0,
            halted_at: 0,
        })
    }

    /// Posts the next report unless one is posted or the ring is halted.
    fn post_report(&mut self) -> bool {
        if self.report_length == 0 || self.posted.is_some() {
            return false;
        }
        let Some(buffer) = self.buffer.as_ref() else {
            return false;
        };
        self.posted = self.ring.normal(buffer.phys(), self.report_length).ok();
        self.posted.is_some()
    }

    fn context(&self, speed: Speed) -> EndpointContext {
        let (dequeue, cycle) = self.ring.dequeue();
        EndpointContext::for_endpoint(&self.descriptor, speed, dequeue, cycle)
    }
}

#[inline(never)]
fn rings(found: &[(u8, descriptor::Endpoint)]) -> Result<KVec<Endpoint>, AllocError> {
    let mut rings = KVec::with_capacity(found.len())?;
    for &(dci, descriptor) in found {
        rings.push(Endpoint::new(dci, descriptor)?)?;
    }
    Ok(rings)
}

#[inline(never)]
fn contexts(rings: &[Endpoint], speed: Speed) -> Result<KVec<(u8, EndpointContext)>, AllocError> {
    let mut contexts = KVec::with_capacity(rings.len())?;
    for ring in rings {
        contexts.push((ring.dci, ring.context(speed)))?;
    }
    Ok(contexts)
}

#[derive(slopos_ostd::SlotFields)]
struct Memory {
    output: Page,
    input: Page,
    /// EP0's data stage.
    control: Page,
    ep0: TransferRing<Page>,
    endpoints: KVec<Endpoint>,
    /// Each [`Posted`]'s data stage, kept as long as the device so an
    /// abandoned request never reads a freed page.
    posted: KVec<Page>,
    /// Each [`Stream`]'s wrapper page and input context, kept as long as the
    /// device for the same reason.
    streams: KVec<StreamPages>,
    /// Each [`Queue`]'s buffers, kept as long as the device for the same
    /// reason.
    buffers: KVec<Page>,
    /// The alternate setting each interface below [`ALTERNATES`] is in.
    alternates: [u8; ALTERNATES],
    gone: bool,
}

struct StreamPages {
    wire: Page,
    input: Page,
}

impl Memory {
    fn new() -> Result<KBox<Self>, AllocError> {
        KBox::try_init(init_struct_with(
            |init: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                let page = || Page::alloc().ok_or(AllocError);
                write_field!(init, output, page()?);
                write_field!(init, input, page()?);
                write_field!(init, control, page()?);
                write_field!(init, ep0, TransferRing::new(page()?));
                write_field!(init, endpoints, KVec::new());
                write_field!(init, posted, KVec::new());
                write_field!(init, streams, KVec::new());
                write_field!(init, buffers, KVec::new());
                write_field!(init, alternates, [0; ALTERNATES]);
                write_field!(init, gone, false);
                Ok(init.finish())
            },
        ))
    }

    fn ring(&mut self, dci: u8) -> Option<&mut TransferRing<Page>> {
        if dci == 1 {
            return Some(&mut self.ep0);
        }
        self.endpoints
            .iter_mut()
            .find(|e| e.dci == dci)
            .map(|e| &mut e.ring)
    }

    fn endpoint(&mut self, dci: u8) -> Option<&mut Endpoint> {
        self.endpoints.iter_mut().find(|e| e.dci == dci)
    }

    fn buffer(&self, dci: u8) -> Option<&Page> {
        if dci == 1 {
            return Some(&self.control);
        }
        self.endpoints
            .iter()
            .find(|e| e.dci == dci)?
            .buffer
            .as_ref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindState {
    Pending,
    /// `claim` indexes the USB claim table.
    Bound {
        driver: &'static str,
        claim: u16,
    },
    Unbound,
    /// The device left before the function was offered.
    Skipped,
    /// The claim waits for the slot's release.
    Released {
        driver: &'static str,
        claim: u16,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Bind {
    pub function: Function,
    pub state: BindState,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Finished {
    pub transfer: bool,
    pub tree: bool,
    pub report: bool,
    pub sink: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbError {
    Transfer(TransferError),
    Timeout,
    Gone,
    /// More than one page.
    TooLong,
    Killed,
    /// The endpoint takes nothing now; try again.
    Busy,
}

#[derive(slopos_ostd::SlotFields)]
pub struct Device {
    pub controller: u8,
    pub slot: u8,
    /// A slot number is reused, a serial never.
    pub serial: u64,
    regs: MmioRegion,
    doorbell: usize,
    contexts: ContextLayout,
    memory: SpinLock<KBox<Memory>>,
    /// A mutex: the tree and the drivers read it with no interrupt masked.
    store: Mutex<Store>,
    /// The drivers share EP0's buffer, one request at a time.
    requests: Mutex<()>,
    node: SpinLock<Option<Node>>,
    /// The tree takes a hub's status-change transfers.
    hub: AtomicBool,
    binds: SpinLock<KVec<Bind>>,
    unbind_asked: AtomicBool,
    unbound: AtomicBool,
    /// Dropped once Disable Slot has completed.
    retired: SpinLock<KVec<ClaimSlot>>,
    /// Taken by the drain once it has released the event lock.
    reporters: SpinLock<KVec<Reporter>>,
    /// Endpoints, by DCI, whose posted report has completed.
    reports_done: AtomicU32,
    /// Endpoints, by DCI, a [`Stream`] drives and recovers: the tree leaves
    /// their halts alone.
    owned: AtomicU32,
    /// Endpoints, by DCI, whose completions go to a [`TransferSink`].
    sinked: AtomicU32,
    sinks: SpinLock<KVec<Sink>>,
    /// Endpoints, by DCI, with a transfer completed or a ring recovered that
    /// their sink has not heard of.
    sinks_done: AtomicU32,
    /// A driver's recovery failed: the tree removes the device and its port
    /// tries it again.
    escalated: AtomicBool,
}

/// Called with the device held, from wherever the event ring is drained, each
/// time a transfer on one of a [`Stream`]'s or [`Queue`]'s endpoints
/// completes, and from the USB thread once a [`Queue`]'s halted ring runs
/// again: it may not block, allocate or log.
pub trait TransferSink: Send + Sync {
    fn completed(&self);
}

struct Sink {
    pipes: u32,
    sink: KArc<dyn TransferSink>,
}

/// Called with each report an endpoint returns, from wherever the event ring
/// is drained: it may not block, allocate or log.
pub trait ReportSink: Send + Sync {
    fn report(&self, report: &[u8]);
}

struct Reporter {
    dci: u8,
    sink: KArc<dyn ReportSink>,
    /// The report, copied out of the page the controller writes.
    copy: Store,
}

impl Device {
    pub(super) fn new(
        controller: u8,
        slot: u8,
        serial: u64,
        regs: MmioRegion,
        doorbell: usize,
        contexts: ContextLayout,
    ) -> Result<KArc<Self>, AllocError> {
        KArc::try_init(init_struct_with(
            move |init: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                let mut retired = KVec::new();
                retired.try_reserve(descriptor::MAX_FUNCTIONS)?;
                Self::install_memory(&init)?;
                write_field!(
                    init,
                    store,
                    Mutex::new(
                        Store::alloc().ok_or(AllocError)?,
                        lock_class!("UsbDevice.store", LOCK_LEVEL_RESOURCE)
                    )
                );
                write_field!(init, controller, controller);
                write_field!(init, slot, slot);
                write_field!(init, serial, serial);
                write_field!(init, regs, regs);
                write_field!(init, doorbell, doorbell);
                write_field!(init, contexts, contexts);
                write_field!(
                    init,
                    requests,
                    Mutex::new((), lock_class!("UsbDevice.requests", LOCK_LEVEL_RESOURCE))
                );
                write_field!(
                    init,
                    node,
                    SpinLock::new(None, lock_class!("UsbDevice.node", LOCK_LEVEL_RESOURCE))
                );
                write_field!(
                    init,
                    binds,
                    SpinLock::new(
                        KVec::new(),
                        lock_class!("UsbDevice.binds", LOCK_LEVEL_RESOURCE)
                    )
                );
                write_field!(init, hub, AtomicBool::new(false));
                write_field!(init, unbind_asked, AtomicBool::new(false));
                write_field!(init, unbound, AtomicBool::new(false));
                write_field!(
                    init,
                    retired,
                    SpinLock::new(
                        retired,
                        lock_class!("UsbDevice.retired", LOCK_LEVEL_RESOURCE)
                    )
                );
                write_field!(
                    init,
                    reporters,
                    SpinLock::new(
                        KVec::new(),
                        lock_class!("UsbDevice.reporters", LOCK_LEVEL_RESOURCE)
                    )
                );
                write_field!(init, reports_done, AtomicU32::new(0));
                write_field!(init, owned, AtomicU32::new(0));
                write_field!(init, sinked, AtomicU32::new(0));
                write_field!(
                    init,
                    sinks,
                    SpinLock::new(
                        KVec::new(),
                        lock_class!("UsbDevice.sinks", LOCK_LEVEL_RESOURCE)
                    )
                );
                write_field!(init, sinks_done, AtomicU32::new(0));
                write_field!(init, escalated, AtomicBool::new(false));
                Ok(init.finish())
            },
        ))
    }

    #[inline(never)]
    fn install_memory(init: &SlotPtr<Self>) -> Result<(), AllocError> {
        let memory = Memory::new()?;
        write_field!(
            init,
            memory,
            SpinLock::new(memory, lock_class!("UsbDevice.memory", LOCK_LEVEL_RESOURCE))
        );
        Ok(())
    }

    pub fn output_phys(&self) -> u64 {
        self.memory.lock().output.phys()
    }

    fn ring_doorbell(&self, memory: &Memory, dci: u8) {
        if !memory.gone {
            self.regs.write::<u32>(self.doorbell, dci.into());
        }
    }

    pub(super) fn address_input(&self, context: &SlotContext, max_packet: u16) -> u64 {
        let mut memory = self.memory.lock();
        let (dequeue, cycle) = memory.ep0.dequeue();
        let ep0 = EndpointContext::control(max_packet, dequeue, cycle);
        let control = InputControlContext {
            add: 0b11,
            ..InputControlContext::default()
        };
        write_input(
            &mut memory.input,
            self.contexts,
            &control,
            Some(context),
            &[(1, ep0)],
        );
        memory.input.phys()
    }

    pub(super) fn evaluate_input(&self, max_packet: u16) -> u64 {
        let mut memory = self.memory.lock();
        let current = read_context(&memory.output, self.contexts.device_endpoint(1));
        let mut ep0 = EndpointContext::decode(&current);
        ep0.max_packet_size = max_packet;
        let control = InputControlContext {
            add: 0b10,
            ..InputControlContext::default()
        };
        write_input(
            &mut memory.input,
            self.contexts,
            &control,
            None,
            &[(1, ep0)],
        );
        memory.input.phys()
    }

    /// Every endpoint of the configuration in the store is given a ring.
    pub(super) fn configure_input(
        &self,
        mut context: SlotContext,
        speed: Speed,
    ) -> Result<u64, AllocError> {
        let found = self.configured()?;
        let rings = rings(&found)?;
        let contexts = contexts(&rings, speed)?;
        context.context_entries = found.iter().map(|&(dci, _)| dci).max().unwrap_or(1);
        Ok(self.install_endpoints(&context, rings, &contexts))
    }

    /// The slot alone, its endpoints left as they are.
    pub(super) fn hub_input(&self, mut context: SlotContext) -> u64 {
        let mut memory = self.memory.lock();
        let current = SlotContext::decode(&read_context(&memory.output, 0));
        context.context_entries = current.context_entries;
        let control = InputControlContext {
            add: 1,
            ..InputControlContext::default()
        };
        write_input(
            &mut memory.input,
            self.contexts,
            &control,
            Some(&context),
            &[],
        );
        memory.input.phys()
    }

    #[inline(never)]
    fn install_endpoints(
        &self,
        context: &SlotContext,
        rings: KVec<Endpoint>,
        contexts: &[(u8, EndpointContext)],
    ) -> u64 {
        let control = InputControlContext {
            add: contexts.iter().fold(1, |add, &(dci, _)| add | 1 << dci),
            ..InputControlContext::default()
        };
        let mut memory = self.memory.lock();
        write_input(
            &mut memory.input,
            self.contexts,
            &control,
            Some(context),
            contexts,
        );
        let replaced = core::mem::replace(&mut memory.endpoints, rings);
        let input = memory.input.phys();
        drop(memory);
        drop(replaced);
        input
    }

    #[inline(never)]
    fn configured(&self) -> Result<KVec<(u8, descriptor::Endpoint)>, AllocError> {
        let mut found = KVec::with_capacity(MAX_ENDPOINTS)?;
        let store = self.store();
        if let Ok(config) = Configuration::parse(&store.bytes()[STORE_CONFIGURATION..]) {
            for_each_configured_endpoint(&config, |dci, endpoint| {
                let _ = found.push((dci, endpoint));
            });
        }
        Ok(found)
    }

    pub(super) fn recovery_dequeue(&self, dci: u8) -> Option<(u64, bool)> {
        Some(self.memory.lock().ring(dci)?.recovery_dequeue())
    }

    /// The tree's; a data stage reads into the control buffer.
    pub(super) fn control(&self, setup: Setup) -> Result<Transfer, PushError> {
        let mut memory = self.memory.lock();
        if memory.gone {
            return Err(PushError::Halted);
        }
        let buffer = memory.control.phys();
        let transfer = memory.ep0.control(setup, buffer)?;
        self.ring_doorbell(&memory, 1);
        Ok(transfer)
    }

    pub(super) fn interrupt_in(&self, dci: u8, length: u16) -> Result<Transfer, PushError> {
        let needs_buffer = {
            let memory = self.memory.lock();
            memory
                .endpoints
                .iter()
                .any(|e| e.dci == dci && e.buffer.is_none())
        };
        let spare = if needs_buffer { Page::alloc() } else { None };
        let mut memory = self.memory.lock();
        if memory.gone {
            return Err(PushError::Halted);
        }
        let endpoint = memory
            .endpoints
            .iter_mut()
            .find(|e| e.dci == dci)
            .ok_or(PushError::Halted)?;
        if endpoint.buffer.is_none() {
            endpoint.buffer = spare;
        }
        let buffer = endpoint.buffer.as_ref().ok_or(PushError::Busy)?.phys();
        let length = u32::from(length).min(PAGE_SIZE as u32);
        let transfer = endpoint.ring.normal(buffer, length)?;
        self.ring_doorbell(&memory, dci);
        Ok(transfer)
    }

    pub(super) fn take(&self, dci: u8, transfer: Transfer) -> Option<TransferResult> {
        self.memory.lock().ring(dci)?.take(transfer)
    }

    /// The USB thread moves the ring past it.
    pub(super) fn abandon(&self, dci: u8, transfer: Transfer) {
        if let Some(ring) = self.memory.lock().ring(dci) {
            ring.abandon(transfer);
        }
        self.note_abandoned();
    }

    fn note_abandoned(&self) {
        if let Some(controller) = super::controller(self.controller) {
            controller.note_work();
        }
        crate::usb::wake();
    }

    pub(super) fn read(&self, dci: u8, out: &mut [u8]) {
        let memory = self.memory.lock();
        match memory.buffer(dci) {
            Some(buffer) => buffer.read_bytes(0, out),
            None => out.fill(0),
        }
    }

    pub(super) fn keep(&self, at: usize, length: usize) {
        let mut store = self.store();
        if let Some(target) = store.bytes_mut().get_mut(at..at + length) {
            self.memory.lock().control.read_bytes(0, target);
        }
    }

    pub fn stored<R>(&self, read: impl FnOnce(&[u8]) -> R) -> R {
        read(self.store().bytes())
    }

    /// A killed task spins for the store, held only for a copy or a read.
    fn store(&self) -> MutexGuard<'_, Store> {
        match self.store.lock() {
            Ok(guard) => guard,
            Err(_) => loop {
                if let Some(guard) = self.store.try_lock() {
                    break guard;
                }
                core::hint::spin_loop();
            },
        }
    }

    pub(super) fn halted(&self) -> u32 {
        let memory = self.memory.lock();
        if memory.gone {
            return 0;
        }
        let halted = memory
            .endpoints
            .iter()
            .filter(|e| e.ring.is_halted())
            .fold(u32::from(memory.ep0.is_halted()) << 1, |bits, e| {
                bits | 1 << e.dci
            });
        halted & !self.owned.load(Ordering::Acquire)
    }

    pub(super) fn take_escalation(&self) -> bool {
        self.escalated.swap(false, Ordering::AcqRel)
    }

    /// The tree's recovery has the ring running again, empty: a [`Queue`]'s
    /// sink hears of it, and a reporting endpoint's runs with a report posted.
    pub(super) fn recovered(&self, dci: u8) {
        self.recover_ring(dci);
        if self.queued() & 1 << dci != 0 {
            self.sinks_done.fetch_or(1 << dci, Ordering::AcqRel);
            self.dispatch_sinks();
        }
    }

    /// Endpoints a [`Queue`] holds: sinked, and recovered by the tree.
    fn queued(&self) -> u32 {
        self.sinked.load(Ordering::Acquire) & !self.owned.load(Ordering::Acquire)
    }

    /// Past [`HALT_RECOVERIES`] a reporting endpoint's reports stop, and a
    /// queue's device is reset.
    fn recover_ring(&self, dci: u8) {
        let queued = self.queued() & 1 << dci != 0;
        let mut memory = self.memory.lock();
        let Some(ring) = memory.ring(dci) else {
            return;
        };
        ring.recovered();
        let gone = memory.gone;
        let Some(endpoint) = memory.endpoint(dci) else {
            return;
        };
        if let Some(posted) = endpoint.posted.take() {
            let _ = endpoint.ring.take(posted);
        }
        if endpoint.report_length == 0 && !queued {
            return;
        }
        let now = slopos_kernel_services::clock::uptime_ms();
        if now.saturating_sub(endpoint.halted_at) > HALT_WINDOW_MS {
            endpoint.recoveries = 0;
        }
        endpoint.halted_at = now;
        endpoint.recoveries = endpoint.recoveries.saturating_add(1);
        if endpoint.recoveries > HALT_RECOVERIES {
            let address = endpoint.descriptor.address;
            let first = endpoint.recoveries == HALT_RECOVERIES + 1;
            drop(memory);
            if queued {
                self.escalated.store(true, Ordering::Release);
                self.note_abandoned();
            }
            if first && let Some(node) = self.node() {
                klog_info!(
                    "USB: {}-{} endpoint {:#04x} keeps halting; {}",
                    self.controller,
                    node.path,
                    address,
                    if queued {
                        "the device is reset"
                    } else {
                        "its reports stop"
                    }
                );
            }
            return;
        }
        if !gone && endpoint.post_report() {
            self.ring_doorbell(&memory, dci);
        }
    }

    pub(super) fn running_endpoints(&self) -> u32 {
        let memory = self.memory.lock();
        (1..32u8)
            .filter(|&dci| {
                let at = self.contexts.device_endpoint(dci);
                EndpointContext::decode(&read_context(&memory.output, at)).state
                    == endpoint_state::RUNNING
            })
            .fold(0, |bits, dci| bits | 1 << dci)
    }

    pub(super) fn mark_gone(&self) {
        self.memory.lock().gone = true;
    }

    pub fn is_gone(&self) -> bool {
        self.memory.lock().gone
    }

    pub(super) fn fail_all(&self, error: TransferError) {
        let mut memory = self.memory.lock();
        memory.ep0.fail_all(error);
        for endpoint in memory.endpoints.iter_mut() {
            endpoint.ring.fail_all(error);
        }
    }

    /// What finishing a transfer leaves to do: the USB thread's work is EP0's,
    /// a hub's or a halt, a report is the drain's, and any other transfer
    /// wakes only its waiter.
    pub(super) fn complete(
        &self,
        dci: u8,
        trb: u64,
        code: CompletionCode,
        residual: u32,
    ) -> Finished {
        use slopos_usb_core::xhci::transfer::Completed;
        let mut memory = self.memory.lock();
        let Some(ring) = memory.ring(dci) else {
            return Finished::default();
        };
        if ring.complete(trb, code, residual) != Completed::Transfer {
            return Finished::default();
        }
        let bit = 1 << dci;
        let owned = self.owned.load(Ordering::Acquire) & bit != 0;
        let sink = self.sinked.load(Ordering::Acquire) & bit != 0;
        let tree = dci == 1 || (ring.is_halted() && !owned) || self.hub.load(Ordering::Acquire);
        let moved = matches!(code, CompletionCode::SUCCESS | CompletionCode::SHORT_PACKET);
        let endpoint = memory.endpoint(dci);
        let report = endpoint.as_ref().is_some_and(|e| e.report_length != 0);
        if let Some(endpoint) = endpoint
            && sink
            && !owned
            && moved
        {
            endpoint.recoveries = 0;
        }
        if report {
            self.reports_done.fetch_or(bit, Ordering::AcqRel);
        }
        if sink {
            self.sinks_done.fetch_or(bit, Ordering::AcqRel);
        }
        Finished {
            transfer: true,
            tree,
            report,
            sink,
        }
    }

    /// Tells each sink what happened on its endpoints. Runs with no event
    /// lock held.
    pub(super) fn dispatch_sinks(&self) {
        let done = self.sinks_done.swap(0, Ordering::AcqRel);
        if done == 0 {
            return;
        }
        for sink in self.sinks.lock().iter().filter(|s| s.pipes & done != 0) {
            sink.sink.completed();
        }
    }

    fn attach_sink(&self, pipes: u32, sink: KArc<dyn TransferSink>) -> Result<(), BoundError> {
        let mut sinks = self.sinks.lock();
        sinks.try_reserve(1).map_err(|_| BoundError::OutOfMemory)?;
        sinks
            .push(Sink { pipes, sink })
            .map_err(|_| BoundError::OutOfMemory)
    }

    fn detach_sink(&self, pipes: u32) {
        self.sinked.fetch_and(!pipes, Ordering::AcqRel);
        let detached = {
            let mut sinks = self.sinks.lock();
            let at = sinks.iter().position(|s| s.pipes == pipes);
            at.map(|at| sinks.swap_remove(at))
        };
        drop(detached);
    }

    pub fn alternates(&self) -> [u8; ALTERNATES] {
        self.memory.lock().alternates
    }

    /// Moves `interface` to alternate setting `alternate` (§4.6.6.1): a
    /// Configure Endpoint gives the new setting's endpoints rings and drops the
    /// old one's, then `SET_INTERFACE` tells the device. Waits for both. A
    /// device that refuses `SET_INTERFACE` has the interface's endpoints
    /// unusable until it is enumerated again.
    #[inline(never)]
    pub(crate) fn select(&self, interface: u8, alternate: u8) -> Result<(), BoundError> {
        let controller = super::controller(self.controller).ok_or(BoundError::Gone)?;
        let (dropped, added) = self.alternate_rings(interface, alternate)?;
        let removed = KVec::with_capacity(dropped.count_ones() as usize)
            .map_err(|_| BoundError::OutOfMemory)?;
        let input = self.alternate_input(dropped, &added)?;
        let ticket = controller
            .submit(Trb::configure_endpoint(input, self.slot, false))
            .map_err(|e| match e {
                SubmitError::Busy => BoundError::Busy,
                SubmitError::Dead => BoundError::Gone,
            })?;
        let waited = super::super::TRANSFERS
            .wait_event_timeout_until(|| controller.take(ticket), COMMAND_MS);
        let aborted = match waited {
            Ok(Ok(done)) if done.code.is_success() => None,
            Ok(_) => return Err(BoundError::Refused),
            Err(abort) => {
                controller.abandon(ticket);
                if abort == WaitAbort::Timeout {
                    controller.note_stuck();
                }
                Some(BoundError::Gone)
            }
        };
        self.install_alternate(interface, alternate, dropped, added, removed);
        if let Some(error) = aborted {
            return Err(error);
        }
        self.request(Setup::set_interface(interface, alternate), None, None)
            .map(|_| ())
            .map_err(|e| match e {
                UsbError::Gone => BoundError::Gone,
                _ => BoundError::Refused,
            })
    }

    /// The DCIs the move drops, and rings for the new setting's endpoints.
    #[inline(never)]
    fn alternate_rings(
        &self,
        interface: u8,
        alternate: u8,
    ) -> Result<(u32, KVec<Endpoint>), BoundError> {
        let (from, held) = {
            let memory = self.memory.lock();
            if memory.gone {
                return Err(BoundError::Gone);
            }
            let from = *memory
                .alternates
                .get(usize::from(interface))
                .ok_or(BoundError::NoSuchEndpoint)?;
            let held = memory.endpoints.iter().fold(0u32, |b, e| b | 1 << e.dci);
            (from, held)
        };
        let mut found = KVec::with_capacity(MAX_ENDPOINTS).map_err(|_| BoundError::OutOfMemory)?;
        let dropped = self
            .stored(|b| {
                let config = Configuration::parse(&b[STORE_CONFIGURATION..]).ok()?;
                let change = alternate_change(&config, interface, from, alternate, held)?;
                for endpoint in config.endpoints(interface, alternate) {
                    let dci =
                        slopos_usb_core::xhci::context::dci(endpoint.number(), endpoint.is_in());
                    let _ = found.push((dci, endpoint));
                }
                Some(change.drop)
            })
            .ok_or(BoundError::NoSuchEndpoint)?;
        let rings = rings(&found).map_err(|_| BoundError::OutOfMemory)?;
        Ok((dropped, rings))
    }

    /// Room for the new rings is made here, so installing them cannot fail
    /// once the controller has them. The tree is done with the input context
    /// once the device is offered.
    #[inline(never)]
    fn alternate_input(&self, dropped: u32, added: &[Endpoint]) -> Result<u64, BoundError> {
        let speed = self.node().ok_or(BoundError::Gone)?.speed;
        let contexts = contexts(added, speed).map_err(|_| BoundError::OutOfMemory)?;
        let mut memory = self.memory.lock();
        if memory
            .endpoints
            .iter()
            .any(|e| dropped & 1 << e.dci != 0 && (e.open || e.ring.outstanding() != 0))
        {
            return Err(BoundError::Busy);
        }
        memory
            .endpoints
            .try_reserve(added.len())
            .map_err(|_| BoundError::OutOfMemory)?;
        let add = added.iter().fold(0u32, |bits, e| bits | 1 << e.dci);
        let kept = memory
            .endpoints
            .iter()
            .fold(0u32, |bits, e| bits | 1 << e.dci)
            & !dropped;
        let mut slot = SlotContext::decode(&read_context(&memory.output, 0));
        slot.context_entries = (31 - (kept | add | 1 << 1).leading_zeros()) as u8;
        let control = InputControlContext {
            drop: dropped,
            add: add | 1,
            ..InputControlContext::default()
        };
        write_input(
            &mut memory.input,
            self.contexts,
            &control,
            Some(&slot),
            &contexts,
        );
        Ok(memory.input.phys())
    }

    /// An abandoned command may still configure the endpoints, so their rings
    /// become the device's whatever its answer.
    fn install_alternate(
        &self,
        interface: u8,
        alternate: u8,
        dropped: u32,
        added: KVec<Endpoint>,
        mut removed: KVec<Endpoint>,
    ) {
        let mut memory = self.memory.lock();
        while let Some(at) = memory
            .endpoints
            .iter()
            .position(|e| dropped & 1 << e.dci != 0)
        {
            let _ = removed.push(memory.endpoints.swap_remove(at));
        }
        for endpoint in added {
            let _ = memory.endpoints.push(endpoint);
        }
        if let Some(selected) = memory.alternates.get_mut(usize::from(interface)) {
            *selected = alternate;
        }
        drop(memory);
        drop(removed);
    }

    /// Hands each completed report to its sink and posts the next. Runs with
    /// no event lock held.
    pub(super) fn dispatch_reports(&self) {
        let done = self.reports_done.swap(0, Ordering::AcqRel);
        if done == 0 {
            return;
        }
        let mut reporters = self.reporters.lock();
        for reporter in reporters.iter_mut().filter(|r| done >> r.dci & 1 != 0) {
            if let Some(length) = self.take_report(reporter.dci, reporter.copy.bytes_mut()) {
                reporter.sink.report(&reporter.copy.bytes()[..length]);
            }
        }
    }

    /// Copies a completed report into `into`, zero-padded to the length
    /// posted, and posts the next. An empty one is dropped.
    fn take_report(&self, dci: u8, into: &mut [u8]) -> Option<usize> {
        let mut memory = self.memory.lock();
        let gone = memory.gone;
        let endpoint = memory.endpoint(dci)?;
        let posted = endpoint.posted?;
        let result = endpoint.ring.take(posted)?;
        endpoint.posted = None;
        let padded = (endpoint.report_length as usize).min(into.len());
        let length = match (result, endpoint.buffer.as_ref()) {
            (Ok(moved @ 1..), Some(buffer)) => {
                endpoint.recoveries = 0;
                let moved = (moved as usize).min(padded);
                buffer.read_bytes(0, &mut into[..moved]);
                into[moved..padded].fill(0);
                Some(padded)
            }
            _ => None,
        };
        if !gone && endpoint.post_report() {
            self.ring_doorbell(&memory, dci);
        }
        length
    }

    /// Keeps a report of `length` bytes posted on the endpoint at `address`,
    /// handing each to `sink`.
    #[inline(never)]
    pub(crate) fn open_reports(
        &self,
        address: u8,
        length: u32,
        sink: KArc<dyn ReportSink>,
    ) -> Result<u8, BoundError> {
        let dci = slopos_usb_core::xhci::context::dci(address & 0x0f, address & 0x80 != 0);
        let copy = Store::alloc().ok_or(BoundError::OutOfMemory)?;
        let buffer = Page::alloc().ok_or(BoundError::OutOfMemory)?;
        let mut reporters = self.reporters.lock();
        reporters
            .try_reserve(1)
            .map_err(|_| BoundError::OutOfMemory)?;
        let mut memory = self.memory.lock();
        if memory.gone {
            return Err(BoundError::Gone);
        }
        let endpoint = memory.endpoint(dci).ok_or(BoundError::NoSuchEndpoint)?;
        if endpoint.open {
            return Err(BoundError::Busy);
        }
        endpoint.open = true;
        endpoint.report_length = length.min(PAGE_SIZE as u32);
        endpoint.recoveries = 0;
        let spare = match endpoint.buffer {
            Some(_) => Some(buffer),
            None => endpoint.buffer.replace(buffer),
        };
        let posted = endpoint.post_report();
        if posted {
            self.ring_doorbell(&memory, dci);
        }
        drop(memory);
        let _ = reporters.push(Reporter { dci, sink, copy });
        drop(reporters);
        drop(spare);
        Ok(dci)
    }

    fn close_reports(&self, dci: u8) {
        let reporter = {
            let mut reporters = self.reporters.lock();
            let at = reporters.iter().position(|r| r.dci == dci);
            at.map(|at| reporters.swap_remove(at))
        };
        drop(reporter);
        let abandoned = {
            let mut memory = self.memory.lock();
            let Some(endpoint) = memory.endpoint(dci) else {
                return;
            };
            endpoint.open = false;
            endpoint.report_length = 0;
            endpoint
                .posted
                .take()
                .inspect(|&posted| endpoint.ring.abandon(posted))
        };
        if abandoned.is_some() {
            self.note_abandoned();
        }
    }

    /// A data page for [`post_control`](Self::post_control), by index.
    fn add_posted_page(&self) -> Result<usize, BoundError> {
        let page = Page::alloc().ok_or(BoundError::OutOfMemory)?;
        let mut memory = self.memory.lock();
        memory
            .posted
            .try_reserve(1)
            .map_err(|_| BoundError::OutOfMemory)?;
        memory
            .posted
            .push(page)
            .map_err(|_| BoundError::OutOfMemory)?;
        Ok(memory.posted.len() - 1)
    }

    /// Abandons every posted report as a halt would leave it, for the USB
    /// thread to move each ring past and post again: how many.
    #[cfg(feature = "test-hooks")]
    pub fn abandon_reports(&self) -> usize {
        let mut memory = self.memory.lock();
        let mut abandoned = 0;
        let mut reposted = 0u32;
        for endpoint in memory.endpoints.iter_mut() {
            if let Some(posted) = endpoint.posted.take() {
                endpoint.ring.abandon(posted);
                if endpoint.post_report() {
                    reposted |= 1 << endpoint.dci;
                }
                abandoned += 1;
            }
        }
        for dci in (1..32u8).filter(|dci| reposted >> dci & 1 != 0) {
            self.ring_doorbell(&memory, dci);
        }
        drop(memory);
        if abandoned != 0 {
            self.note_abandoned();
        }
        abandoned
    }

    /// Every reporting endpoint runs with a report posted.
    #[cfg(feature = "test-hooks")]
    pub fn reports_posted(&self) -> bool {
        let memory = self.memory.lock();
        memory
            .endpoints
            .iter()
            .filter(|e| e.report_length != 0)
            .all(|e| e.posted.is_some() && !e.ring.is_halted())
    }

    /// A control request whose data stage is `data`, written to posted page
    /// `page` and left for [`posted`](Self::posted) to collect: for a thread
    /// that may not wait.
    pub(crate) fn post_control(
        &self,
        setup: Setup,
        page: usize,
        data: &[u8],
    ) -> Result<Transfer, UsbError> {
        let mut memory = self.memory.lock();
        if memory.gone {
            return Err(UsbError::Gone);
        }
        if !memory.ep0.accepts() {
            return Err(UsbError::Busy);
        }
        let buffer = memory.posted.get_mut(page).ok_or(UsbError::Gone)?;
        buffer.write_bytes(0, data);
        let phys = buffer.phys();
        let transfer = memory
            .ep0
            .control(setup, phys)
            .map_err(|_| UsbError::Busy)?;
        self.ring_doorbell(&memory, 1);
        Ok(transfer)
    }

    pub(crate) fn posted(&self, transfer: Transfer) -> Option<TransferResult> {
        self.memory.lock().ep0.take(transfer)
    }

    /// The `n`th endpoint, EP0 first: DCI, address and transfers outstanding.
    pub fn queue(&self, n: usize) -> Option<(u8, u8, usize)> {
        let memory = self.memory.lock();
        match n.checked_sub(1) {
            None => Some((1, 0, memory.ep0.outstanding())),
            Some(i) => memory
                .endpoints
                .get(i)
                .map(|e| (e.dci, e.descriptor.address, e.ring.outstanding())),
        }
    }

    pub(in crate::usb) fn set_node(&self, node: Node) {
        self.hub.store(node.hub, Ordering::Release);
        *self.node.lock() = Some(node);
    }

    pub fn node(&self) -> Option<Node> {
        *self.node.lock()
    }

    pub(in crate::usb) fn set_binds(&self, binds: KVec<Bind>) {
        let replaced = core::mem::replace(&mut *self.binds.lock(), binds);
        drop(replaced);
    }

    pub fn binds(&self, mut visit: impl FnMut(usize, &Bind)) {
        for (index, bind) in self.binds.lock().iter().enumerate() {
            visit(index, bind);
        }
    }

    pub fn bind(&self, index: usize) -> Option<Bind> {
        self.binds.lock().get(index).copied()
    }

    pub(in crate::usb) fn set_bind(&self, index: usize, state: BindState) {
        if let Some(bind) = self.binds.lock().get_mut(index) {
            bind.state = state;
        }
    }

    pub fn resolved(&self) -> bool {
        self.binds
            .lock()
            .iter()
            .all(|b| b.state != BindState::Pending)
    }

    pub(in crate::usb) fn ask_unbind(&self) {
        self.unbind_asked.store(true, Ordering::Release);
    }

    /// A `true` hands the drivers' removals to the caller.
    pub(in crate::usb) fn take_unbind(&self) -> bool {
        self.unbind_asked.swap(false, Ordering::AcqRel)
    }

    pub(in crate::usb) fn set_unbound(&self) {
        self.unbound.store(true, Ordering::Release);
    }

    pub fn is_unbound(&self) -> bool {
        self.unbound.load(Ordering::Acquire)
    }

    /// The room was reserved when the device was made.
    pub(in crate::usb) fn retire(&self, claim: ClaimSlot) {
        let _ = self.retired.lock().push(claim);
    }

    pub(in crate::usb) fn take_retired(&self) -> KVec<ClaimSlot> {
        core::mem::take(&mut *self.retired.lock())
    }

    fn request(
        &self,
        setup: Setup,
        out: Option<&[u8]>,
        into: Option<&mut [u8]>,
    ) -> Result<usize, UsbError> {
        let _turn = self.requests.lock().map_err(|_| UsbError::Killed)?;
        if usize::from(setup.length) > PAGE_SIZE {
            return Err(UsbError::TooLong);
        }
        let ticket = self.submit_when_running(
            |memory| {
                if !memory.ep0.accepts() {
                    return Err(PushError::Busy);
                }
                if let Some(data) = out {
                    memory.control.write_bytes(0, data);
                }
                let buffer = memory.control.phys();
                memory.ep0.control(setup, buffer)
            },
            1,
        )?;
        let length = self.wait(1, ticket, CONTROL_MS)?;
        if let Some(into) = into {
            let length = (length as usize).min(into.len());
            self.memory
                .lock()
                .control
                .read_bytes(0, &mut into[..length]);
            return Ok(length);
        }
        Ok(length as usize)
    }

    /// A halted ring is waited for while the USB thread recovers it.
    fn submit_when_running(
        &self,
        mut push: impl FnMut(&mut Memory) -> Result<Transfer, PushError>,
        dci: u8,
    ) -> Result<Transfer, UsbError> {
        let submitted = super::super::TRANSFERS.wait_event_timeout_until(
            || {
                let mut memory = self.memory.lock();
                if memory.gone {
                    return Some(Err(UsbError::Gone));
                }
                match push(&mut memory) {
                    Ok(ticket) => {
                        self.ring_doorbell(&memory, dci);
                        Some(Ok(ticket))
                    }
                    Err(PushError::Halted | PushError::Busy) => None,
                }
            },
            CONTROL_MS,
        );
        match submitted {
            Ok(result) => result,
            Err(WaitAbort::Timeout) => Err(UsbError::Timeout),
            Err(_) => Err(UsbError::Killed),
        }
    }

    fn wait(&self, dci: u8, ticket: Transfer, timeout_ms: u64) -> Result<u32, UsbError> {
        let result = super::super::TRANSFERS.wait_event_timeout_until(
            || self.memory.lock().ring(dci).and_then(|r| r.take(ticket)),
            timeout_ms,
        );
        match result {
            Ok(Ok(length)) => Ok(length),
            Ok(Err(TransferError::Gone | TransferError::Dead)) => Err(UsbError::Gone),
            Ok(Err(_)) if self.is_gone() => Err(UsbError::Gone),
            Ok(Err(error)) => Err(UsbError::Transfer(error)),
            Err(abort) => {
                self.abandon(dci, ticket);
                Err(if abort == WaitAbort::Timeout {
                    UsbError::Timeout
                } else {
                    UsbError::Killed
                })
            }
        }
    }
}

pub struct Control {
    device: KArc<Device>,
}

impl Control {
    pub(crate) fn new(device: KArc<Device>) -> Self {
        Self { device }
    }

    pub fn read(&self, setup: Setup, into: &mut [u8]) -> Result<usize, UsbError> {
        self.device.request(setup, None, Some(into))
    }

    pub fn write(&self, mut setup: Setup, data: &[u8]) -> Result<(), UsbError> {
        setup.length = u16::try_from(data.len()).map_err(|_| UsbError::TooLong)?;
        self.device.request(setup, Some(data), None).map(|_| ())
    }
}

/// One endpoint, a page at a time.
pub struct Pipe {
    device: KArc<Device>,
    dci: u8,
    endpoint: descriptor::Endpoint,
    busy: Mutex<()>,
}

impl Pipe {
    /// Unless no endpoint of the configuration is at `address`, or another
    /// pipe holds it.
    pub(crate) fn open(device: KArc<Device>, address: u8) -> Result<Self, BoundError> {
        let dci = slopos_usb_core::xhci::context::dci(address & 0x0f, address & 0x80 != 0);
        let page = Page::alloc().ok_or(BoundError::OutOfMemory)?;
        let (endpoint, spare) = {
            let mut memory = device.memory.lock();
            let endpoint = memory
                .endpoints
                .iter_mut()
                .find(|e| e.dci == dci)
                .ok_or(BoundError::NoSuchEndpoint)?;
            if endpoint.open {
                return Err(BoundError::Busy);
            }
            endpoint.open = true;
            let spare = match endpoint.buffer {
                None => {
                    endpoint.buffer = Some(page);
                    None
                }
                Some(_) => Some(page),
            };
            (endpoint.descriptor, spare)
        };
        drop(spare);
        Ok(Self {
            device,
            dci,
            endpoint,
            busy: Mutex::new((), lock_class!("UsbPipe.busy", LOCK_LEVEL_RESOURCE)),
        })
    }

    pub fn endpoint(&self) -> &descriptor::Endpoint {
        &self.endpoint
    }

    /// At most a page.
    pub fn read(&self, into: &mut [u8], timeout_ms: u64) -> Result<usize, UsbError> {
        let length = into.len();
        let _busy = self.busy.lock().map_err(|_| UsbError::Killed)?;
        let moved = self.transfer(None, length, timeout_ms)?.min(length);
        if let Some(buffer) = self.device.memory.lock().buffer(self.dci) {
            buffer.read_bytes(0, &mut into[..moved]);
        }
        Ok(moved)
    }

    /// At most a page.
    pub fn write(&self, data: &[u8], timeout_ms: u64) -> Result<usize, UsbError> {
        let _busy = self.busy.lock().map_err(|_| UsbError::Killed)?;
        self.transfer(Some(data), data.len(), timeout_ms)
    }

    #[cfg(feature = "test-hooks")]
    pub fn is_idle(&self) -> bool {
        self.device
            .memory
            .lock()
            .ring(self.dci)
            .is_some_and(|ring| ring.accepts() && ring.outstanding() == 0)
    }

    /// The caller holds `busy`.
    fn transfer(
        &self,
        out: Option<&[u8]>,
        length: usize,
        timeout_ms: u64,
    ) -> Result<usize, UsbError> {
        if length > PAGE_SIZE {
            return Err(UsbError::TooLong);
        }
        let dci = self.dci;
        let ticket = self.device.submit_when_running(
            |memory| {
                let endpoint = memory
                    .endpoints
                    .iter_mut()
                    .find(|e| e.dci == dci)
                    .ok_or(PushError::Halted)?;
                if !endpoint.ring.accepts() {
                    return Err(PushError::Busy);
                }
                let buffer = endpoint.buffer.as_mut().ok_or(PushError::Halted)?;
                if let Some(data) = out {
                    buffer.write_bytes(0, data);
                }
                let phys = buffer.phys();
                endpoint.ring.normal(phys, length as u32)
            },
            dci,
        )?;
        self.device
            .wait(dci, ticket, timeout_ms)
            .map(|n| n as usize)
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        let mut memory = self.device.memory.lock();
        if let Some(endpoint) = memory.endpoints.iter_mut().find(|e| e.dci == self.dci) {
            endpoint.open = false;
        }
    }
}

/// An endpoint that keeps a report posted, until this is dropped.
pub struct Reports {
    device: KArc<Device>,
    dci: u8,
}

impl Reports {
    pub(crate) fn open(
        device: KArc<Device>,
        address: u8,
        length: u32,
        sink: KArc<dyn ReportSink>,
    ) -> Result<Self, BoundError> {
        let dci = device.open_reports(address, length, sink)?;
        Ok(Self { device, dci })
    }
}

impl Drop for Reports {
    fn drop(&mut self) {
        self.device.close_reports(self.dci);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    Idle,
    Out,
    Done(TransferResult),
}

/// Control requests sent one at a time and collected later, for the USB
/// thread, which never waits on a completion.
pub struct Posted {
    device: KArc<Device>,
    page: usize,
    /// The request out, and when it was sent.
    out: SpinLock<Option<(Transfer, u64)>>,
}

impl Posted {
    pub(crate) fn new(device: KArc<Device>) -> Result<Self, BoundError> {
        let page = device.add_posted_page()?;
        Ok(Self {
            device,
            page,
            out: SpinLock::new(None, lock_class!("UsbPosted.out", LOCK_LEVEL_RESOURCE)),
        })
    }

    /// `data` is the request's data stage, which `setup` must size.
    pub fn send(&self, setup: Setup, data: &[u8]) -> Result<(), UsbError> {
        if usize::from(setup.length) != data.len() || data.len() > PAGE_SIZE {
            return Err(UsbError::TooLong);
        }
        let mut out = self.out.lock();
        if out.is_some() {
            return Err(UsbError::Busy);
        }
        let transfer = self.device.post_control(setup, self.page, data)?;
        *out = Some((transfer, slopos_kernel_services::clock::uptime_ms()));
        Ok(())
    }

    /// A request still out after [`CONTROL_MS`] is abandoned, as a waited one
    /// is, and fails as a transaction error.
    pub fn poll(&self) -> Poll {
        let mut out = self.out.lock();
        let Some((transfer, sent)) = *out else {
            return Poll::Idle;
        };
        match self.device.posted(transfer) {
            Some(result) => {
                *out = None;
                Poll::Done(result)
            }
            None if self.device.is_gone() => {
                *out = None;
                Poll::Done(Err(TransferError::Gone))
            }
            None if slopos_kernel_services::clock::uptime_ms().saturating_sub(sent)
                >= CONTROL_MS =>
            {
                *out = None;
                drop(out);
                self.device.abandon(1, transfer);
                Poll::Done(Err(TransferError::Transaction))
            }
            None => Poll::Out,
        }
    }
}

impl Drop for Posted {
    fn drop(&mut self) {
        if let Some((transfer, _)) = self.out.lock().take() {
            self.device.abandon(1, transfer);
        }
    }
}

/// A bulk IN and a bulk OUT endpoint a driver drives itself, never waiting:
/// TDs pushed from any context, each completion handed to its sink where the
/// event ring is drained, and their halts left to the driver to recover.
pub struct Stream {
    device: KArc<Device>,
    pipes: [(u8, u16); 2],
    index: usize,
}

pub const STREAM_IN: usize = 0;
pub const STREAM_OUT: usize = 1;

impl Stream {
    /// The hub to clear when the device is reached through a translator.
    pub fn translator(&self) -> Option<Translator> {
        let (hub, _) = self.device.node()?.tt?;
        let hub = super::controller(self.device.controller)?.device(hub)?;
        Some(Translator { hub })
    }

    /// Unless either endpoint is not a bulk endpoint of the configuration in
    /// that direction, or another handle holds it.
    #[inline(never)]
    pub(crate) fn open(device: KArc<Device>, addresses: [u8; 2]) -> Result<Self, BoundError> {
        let wire = Page::alloc().ok_or(BoundError::OutOfMemory)?;
        let input = Page::alloc().ok_or(BoundError::OutOfMemory)?;
        let mut memory = device.memory.lock();
        if memory.gone {
            return Err(BoundError::Gone);
        }
        memory
            .streams
            .try_reserve(1)
            .map_err(|_| BoundError::OutOfMemory)?;
        let mut pipes = [(0u8, 0u16); 2];
        for (pipe, address) in pipes.iter_mut().zip(addresses) {
            let dci = slopos_usb_core::xhci::context::dci(address & 0x0f, address & 0x80 != 0);
            let endpoint = memory.endpoint(dci).ok_or(BoundError::NoSuchEndpoint)?;
            let bulk = endpoint.descriptor.transfer_type() == descriptor::TransferType::Bulk;
            if !bulk || endpoint.open {
                return Err(BoundError::Busy);
            }
            *pipe = (dci, endpoint.descriptor.max_packet_size());
        }
        if pipes[0].0 == pipes[1].0 || pipes[STREAM_IN].0 % 2 == 0 || pipes[STREAM_OUT].0 % 2 == 1 {
            return Err(BoundError::NoSuchEndpoint);
        }
        for (dci, _) in pipes {
            if let Some(endpoint) = memory.endpoint(dci) {
                endpoint.open = true;
            }
        }
        let _ = memory.streams.push(StreamPages { wire, input });
        let index = memory.streams.len() - 1;
        drop(memory);
        let mask = Self::mask_of(&pipes);
        device.owned.fetch_or(mask, Ordering::AcqRel);
        device.sinked.fetch_or(mask, Ordering::AcqRel);
        Ok(Self {
            device,
            pipes,
            index,
        })
    }

    fn mask_of(pipes: &[(u8, u16); 2]) -> u32 {
        pipes.iter().fold(0, |mask, &(dci, _)| mask | 1 << dci)
    }

    /// `sink` hears of every completion on the stream's endpoints from here
    /// until the stream is dropped.
    pub fn attach(&self, sink: KArc<dyn TransferSink>) -> Result<(), BoundError> {
        self.device.attach_sink(Self::mask_of(&self.pipes), sink)
    }

    /// The address the controller gave the device.
    pub fn address(&self) -> u8 {
        let memory = self.device.memory.lock();
        SlotContext::decode(&read_context(&memory.output, 0)).address
    }

    pub fn is_gone(&self) -> bool {
        self.device.is_gone()
    }

    fn dci(&self, pipe: usize) -> u8 {
        self.pipes[pipe & 1].0
    }

    pub fn wire_phys(&self) -> u64 {
        self.device.memory.lock().streams[self.index].wire.phys()
    }

    pub fn write_wire(&self, offset: usize, bytes: &[u8]) {
        self.device.memory.lock().streams[self.index]
            .wire
            .write_bytes(offset, bytes);
    }

    pub fn read_wire(&self, offset: usize, out: &mut [u8]) {
        self.device.memory.lock().streams[self.index]
            .wire
            .read_bytes(offset, out);
    }

    /// One TD of `length` bytes over `pages` on pipe [`STREAM_IN`] or
    /// [`STREAM_OUT`], its doorbell rung.
    pub fn push(&self, pipe: usize, pages: &[u64], length: u32) -> Result<Transfer, PushError> {
        let (dci, max_packet) = self.pipes[pipe & 1];
        let mut memory = self.device.memory.lock();
        if memory.gone {
            return Err(PushError::Halted);
        }
        let ring = memory.ring(dci).ok_or(PushError::Halted)?;
        let transfer = ring.bulk(pages, length, max_packet)?;
        self.device.ring_doorbell(&memory, dci);
        Ok(transfer)
    }

    pub fn result(&self, pipe: usize, transfer: Transfer) -> Option<TransferResult> {
        self.device.take(self.dci(pipe), transfer)
    }

    pub fn abandon(&self, pipe: usize, transfer: Transfer) {
        self.device.abandon(self.dci(pipe), transfer);
    }

    /// The ring of the pipe runs again, empty.
    pub fn recovered(&self, pipe: usize) {
        self.device.recover_ring(self.dci(pipe));
    }

    /// A request with no data stage on the device's EP0.
    pub fn control(&self, setup: Setup) -> Result<Transfer, PushError> {
        self.device.control(setup)
    }

    pub fn control_result(&self, transfer: Transfer) -> Option<TransferResult> {
        self.device.take(1, transfer)
    }

    pub fn abandon_control(&self, transfer: Transfer) {
        self.device.abandon(1, transfer);
    }

    pub fn reset_endpoint(&self, pipe: usize) -> Result<Ticket, SubmitError> {
        let trb = Trb::reset_endpoint(self.device.slot, self.dci(pipe), false);
        self.submit(trb)
    }

    pub fn stop_endpoint(&self, pipe: usize) -> Result<Ticket, SubmitError> {
        let trb = Trb::stop_endpoint(self.device.slot, self.dci(pipe), false);
        self.submit(trb)
    }

    /// Configure Endpoint dropping and adding the pipes `mask` names, by
    /// `1 << pipe`, each at its ring's enqueue pointer.
    pub fn readd(&self, mask: u8) -> Result<Ticket, SubmitError> {
        let mut endpoints = [(0u8, 0u64, false); MAX_READDED];
        let mut count = 0;
        let input = {
            let mut memory = self.device.memory.lock();
            if memory.gone {
                return Err(SubmitError::Dead);
            }
            for pipe in [STREAM_IN, STREAM_OUT] {
                let dci = self.dci(pipe);
                if mask & 1 << pipe == 0 {
                    continue;
                }
                let Some(ring) = memory.ring(dci) else {
                    continue;
                };
                let (dequeue, cycle) = ring.recovery_dequeue();
                endpoints[count] = (dci, dequeue, cycle);
                count += 1;
            }
            let Memory {
                output, streams, ..
            } = &mut **memory;
            let input = &mut streams[self.index].input;
            write_readded(output, input, self.device.contexts, &endpoints[..count]);
            input.phys()
        };
        self.submit(Trb::configure_endpoint(input, self.device.slot, false))
    }

    fn submit(&self, trb: Trb) -> Result<Ticket, SubmitError> {
        super::controller(self.device.controller)
            .ok_or(SubmitError::Dead)?
            .submit(trb)
    }

    pub fn command_result(&self, ticket: Ticket) -> Option<CommandResult> {
        super::controller(self.device.controller)?.take(ticket)
    }

    pub fn abandon_command(&self, ticket: Ticket) {
        if let Some(controller) = super::controller(self.device.controller) {
            controller.abandon(ticket);
        }
    }

    /// A command never completed: the controller is taken for dead.
    pub fn stuck(&self) {
        if let Some(controller) = super::controller(self.device.controller) {
            controller.note_stuck();
        }
    }

    /// Recovery failed: the device is removed and its port tries it again.
    pub fn escalate(&self) {
        self.device.escalated.store(true, Ordering::Release);
        self.device.note_abandoned();
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let mask = Self::mask_of(&self.pipes);
        self.device.detach_sink(mask);
        self.device.owned.fetch_and(!mask, Ordering::AcqRel);
        let mut memory = self.device.memory.lock();
        for (dci, _) in self.pipes {
            if let Some(endpoint) = memory.endpoint(dci) {
                endpoint.open = false;
            }
        }
    }
}

/// A bulk endpoint a driver keeps transfers on without waiting, each over
/// one of its buffers: its sink hears of every completion, and of its ring
/// running again once the tree has recovered a halt.
pub struct Queue {
    device: KArc<Device>,
    dci: u8,
    max_packet: u16,
    first_page: usize,
    buffers: usize,
    pages: usize,
}

impl Queue {
    /// `buffers` buffers of `pages` pages each, unless no bulk endpoint of
    /// the device's current settings is at `address`, or another handle holds
    /// it.
    #[inline(never)]
    pub(crate) fn open(
        device: KArc<Device>,
        address: u8,
        buffers: usize,
        pages: usize,
    ) -> Result<Self, BoundError> {
        let pages = pages.clamp(1, MAX_TD_PAGES);
        let dci = slopos_usb_core::xhci::context::dci(address & 0x0f, address & 0x80 != 0);
        let mut allocated =
            KVec::with_capacity(buffers * pages).map_err(|_| BoundError::OutOfMemory)?;
        for _ in 0..buffers * pages {
            let page = Page::alloc().ok_or(BoundError::OutOfMemory)?;
            let _ = allocated.push(page);
        }
        let mut memory = device.memory.lock();
        if memory.gone {
            return Err(BoundError::Gone);
        }
        memory
            .buffers
            .try_reserve(allocated.len())
            .map_err(|_| BoundError::OutOfMemory)?;
        let endpoint = memory.endpoint(dci).ok_or(BoundError::NoSuchEndpoint)?;
        if endpoint.descriptor.transfer_type() != descriptor::TransferType::Bulk {
            return Err(BoundError::NoSuchEndpoint);
        }
        if endpoint.open {
            return Err(BoundError::Busy);
        }
        endpoint.open = true;
        let max_packet = endpoint.descriptor.max_packet_size();
        let first_page = memory.buffers.len();
        for page in allocated {
            let _ = memory.buffers.push(page);
        }
        drop(memory);
        device.sinked.fetch_or(1 << dci, Ordering::AcqRel);
        Ok(Self {
            device,
            dci,
            max_packet,
            first_page,
            buffers,
            pages,
        })
    }

    /// `sink` hears of the endpoint from here until the queue is dropped.
    pub fn attach(&self, sink: KArc<dyn TransferSink>) -> Result<(), BoundError> {
        self.device.attach_sink(1 << self.dci, sink)
    }

    pub fn buffers(&self) -> usize {
        self.buffers
    }

    pub fn buffer_bytes(&self) -> usize {
        self.pages * PAGE_SIZE
    }

    pub fn max_packet(&self) -> u16 {
        self.max_packet
    }

    pub fn is_gone(&self) -> bool {
        self.device.is_gone()
    }

    /// One TD of `length` bytes over buffer `buffer`, its doorbell rung.
    pub fn push(&self, buffer: usize, length: u32) -> Result<Transfer, PushError> {
        if buffer >= self.buffers {
            return Err(PushError::Busy);
        }
        let mut pages = [0u64; MAX_TD_PAGES];
        let mut memory = self.device.memory.lock();
        if memory.gone {
            return Err(PushError::Halted);
        }
        let at = self.first_page + buffer * self.pages;
        for (phys, page) in pages.iter_mut().zip(&memory.buffers[at..at + self.pages]) {
            *phys = page.phys();
        }
        let ring = memory.ring(self.dci).ok_or(PushError::Halted)?;
        let transfer = ring.bulk(&pages[..self.pages], length, self.max_packet)?;
        self.device.ring_doorbell(&memory, self.dci);
        Ok(transfer)
    }

    pub fn result(&self, transfer: Transfer) -> Option<TransferResult> {
        self.device.take(self.dci, transfer)
    }

    /// Bytes past the buffer's end are not written.
    pub fn write(&self, buffer: usize, offset: usize, bytes: &[u8]) {
        let mut memory = self.device.memory.lock();
        let mut done = 0;
        while let Some((page, at, len)) = self.locate(buffer, offset + done, bytes.len() - done) {
            memory.buffers[page].write_bytes(at, &bytes[done..done + len]);
            done += len;
        }
    }

    /// Bytes past the buffer's end read as zero.
    pub fn read(&self, buffer: usize, offset: usize, out: &mut [u8]) {
        let memory = self.device.memory.lock();
        let mut done = 0;
        while let Some((page, at, len)) = self.locate(buffer, offset + done, out.len() - done) {
            memory.buffers[page].read_bytes(at, &mut out[done..done + len]);
            done += len;
        }
        out[done..].fill(0);
    }

    /// The page among the device's holding byte `offset` of buffer `buffer`,
    /// where in it, and how many of the next `len` bytes it holds.
    fn locate(&self, buffer: usize, offset: usize, len: usize) -> Option<(usize, usize, usize)> {
        if len == 0 || buffer >= self.buffers || offset >= self.buffer_bytes() {
            return None;
        }
        let at = offset % PAGE_SIZE;
        let page = self.first_page + buffer * self.pages + offset / PAGE_SIZE;
        Some((page, at, len.min(PAGE_SIZE - at)))
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        self.device.detach_sink(1 << self.dci);
        if let Some(endpoint) = self.device.memory.lock().endpoint(self.dci) {
            endpoint.open = false;
        }
    }
}

/// The high-speed hub whose transaction translator a full- or low-speed
/// device is reached through: requests with no data stage on its EP0.
pub struct Translator {
    hub: KArc<Device>,
}

impl Translator {
    pub fn control(&self, setup: Setup) -> Result<Transfer, PushError> {
        self.hub.control(setup)
    }

    pub fn control_result(&self, transfer: Transfer) -> Option<TransferResult> {
        self.hub.take(1, transfer)
    }

    pub fn abandon_control(&self, transfer: Transfer) {
        self.hub.abandon(1, transfer);
    }
}
