//! One device in a slot: its contexts, rings and buffers behind the lock the
//! drain and the threads share, its descriptors, its bindings, and the
//! blocking transfers a bound driver makes.

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_mm::mmio::MmioRegion;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, Mutex, MutexGuard, SpinLock, WaitAbort};
use slopos_ostd::{KArc, KBox, KVec, lock_class, write_field};
use slopos_usb_core::bus::{
    MAX_ENDPOINTS, Node, STORE_CONFIGURATION, for_each_configured_endpoint,
};
use slopos_usb_core::device::Speed;
use slopos_usb_core::device::descriptor::{self, Configuration, Function};
use slopos_usb_core::device::request::Setup;
use slopos_usb_core::xhci::context::{
    ContextLayout, EndpointContext, InputControlContext, SlotContext, endpoint_state, read_context,
    write_input,
};
use slopos_usb_core::xhci::memory::PAGE_SIZE;
use slopos_usb_core::xhci::transfer::{PushError, Transfer, TransferError, TransferResult};
use slopos_usb_core::xhci::{CompletionCode, DmaPage, TransferRing};

use super::page::{Page, Store};
use crate::driver_core::bound::BoundError;
use crate::driver_core::bus::ClaimSlot;

/// USB 2.0 §9.2.6.4.
pub const CONTROL_MS: u64 = 5000;

struct Endpoint {
    dci: u8,
    descriptor: descriptor::Endpoint,
    ring: TransferRing<Page>,
    /// Given out with the pipe, or the tree's first transfer.
    buffer: Option<Page>,
    open: bool,
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
        })
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
    gone: bool,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbError {
    Transfer(TransferError),
    Timeout,
    Gone,
    /// More than one page.
    TooLong,
    Killed,
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
        memory
            .endpoints
            .iter()
            .filter(|e| e.ring.is_halted())
            .fold(u32::from(memory.ep0.is_halted()) << 1, |bits, e| {
                bits | 1 << e.dci
            })
    }

    pub(super) fn recovered(&self, dci: u8) {
        if let Some(ring) = self.memory.lock().ring(dci) {
            ring.recovered();
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

    /// Whether it finished a transfer, and whether that is the USB thread's:
    /// EP0's, a hub's, or a halt. Other transfers wake only their waiters.
    pub(super) fn complete(
        &self,
        dci: u8,
        trb: u64,
        code: CompletionCode,
        residual: u32,
    ) -> (bool, bool) {
        use slopos_usb_core::xhci::transfer::Completed;
        let mut memory = self.memory.lock();
        let Some(ring) = memory.ring(dci) else {
            return (false, false);
        };
        let finished = ring.complete(trb, code, residual) == Completed::Transfer;
        let tree = dci == 1 || ring.is_halted() || self.hub.load(Ordering::Acquire);
        (finished, finished && tree)
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
                if let Some(controller) = super::controller(self.controller) {
                    controller.note_work();
                }
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
